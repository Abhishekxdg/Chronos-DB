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

/// A scan run again and again with the same conditions (an agent's loop) is kept once: past `MOST_FILTERS` (1,000)
/// filtered scans of one table it would otherwise count as reading the whole table, and any change to it would hold
/// the merge. Issue #29.
#[test]
fn the_same_filtered_scan_is_kept_once() {
    let db = Db::new();
    run(&db, MAIN, "create table leads (id int primary key, owner text, region text)", &[]).unwrap();
    run(&db, MAIN, "insert into leads select g, 'o' || (g % 25), 'r' || (g % 7) from generate_series(1, 2000) g", &[])
        .unwrap();
    run(&db, MAIN, "create world loop with (check_reads = true)", &[]).unwrap();
    let mut s = Session::new("loop");
    // 1,500 scans over 10 distinct conditions (owner isn't indexed: each is a filtered scan of the table)
    for i in 0..1500 {
        s.run(&db, &format!("select count(*) from leads where owner = 'o{}'", i % 10), &[]).unwrap();
    }
    s.run(&db, "update leads set region = 'done' where id = 25", &[]).unwrap(); // owner o0
    // rows it never read (another owner) change in main: still merges
    run(&db, MAIN, "update leads set region = 'x' where owner = 'o13'", &[]).unwrap();
    let plan = db.merge_preview("loop", &Default::default()).unwrap();
    assert!(plan.blocked.is_none(), "{:?}", plan.blocked);
    // one it read (owner o7) holds it, as before
    run(&db, MAIN, "update leads set region = 'y' where id = 7", &[]).unwrap();
    let e = run(&db, MAIN, "merge world loop", &[]).unwrap_err();
    assert!(e.code == "40001" && e.message.contains("leads/7"), "{}", e.message);
}

/// CHECK READS: what a world that checks its reads read that changed since its fork, asked between steps without
/// merging, and with nothing changed (the world's view stays as it was). An empty result read counts: a row added
/// later that the read would have found is listed. Issue #35.
#[test]
fn check_reads_says_what_changed_without_merging() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table suppressed (email text primary key); create table leads (id int primary key, email text, status text);
         insert into leads values (1, 'ada@x.com', 'new'), (2, 'bo@x.com', 'new')",
        &[],
    )
    .unwrap();
    run(&db, MAIN, "create world w with (check_reads = true)", &[]).unwrap();
    let mut s = Session::new("w");
    let count = |s: &mut Session, q: &str| s.run(&db, q, &[]).unwrap().pop().unwrap().rows[0][0].to_text();
    assert_eq!(count(&mut s, "select count(*) from suppressed where email = 'ada@x.com'"), "0");
    s.run(&db, "update leads set status = 'ready_to_contact' where id = 1", &[]).unwrap();
    let check = |s: &mut Session, q: &str| s.run(&db, q, &[]).unwrap().pop().unwrap();
    // nothing moved: no rows, from the world itself or from main naming it
    assert_eq!(check(&mut s, "check reads").rows.len(), 0);
    assert_eq!(run(&db, MAIN, "check reads of world w", &[]).unwrap().pop().unwrap().rows.len(), 0);
    // a row it couldn't have read changes: still nothing
    run(&db, MAIN, "update leads set status = 'won' where id = 2; insert into suppressed values ('zed@x.com')", &[])
        .unwrap();
    assert_eq!(check(&mut s, "check reads").rows.len(), 0);
    // the email it found no row for is added on main: listed, against main, and nothing changed in the world
    run(&db, MAIN, "insert into suppressed values ('ada@x.com')", &[]).unwrap();
    let found = check(&mut s, "check reads");
    let cols: Vec<&str> = found.columns.as_ref().unwrap().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(cols, ["checked_against", "changed"]);
    assert_eq!(found.rows.len(), 1, "{:?}", found.rows);
    assert_eq!(found.rows[0][0].to_text(), "main");
    assert!(found.rows[0][1].to_text().contains("changed since it forked"), "{:?}", found.rows);
    assert_eq!(count(&mut s, "select count(*) from suppressed where email = 'ada@x.com'"), "0", "no rebase");
    // its own write isn't a stale read; the merge is held for the same read CHECK READS listed
    let e = run(&db, MAIN, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    // a world that doesn't check its reads, and main, are refused with what to do
    run(&db, MAIN, "create world plain", &[]).unwrap();
    let e = run(&db, MAIN, "check reads of world plain", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    assert!(e.message.contains("check_reads = true"), "{}", e.message);
    assert!(run(&db, MAIN, "check reads", &[]).is_err(), "main has no fork point");
}

/// What a world read survives a server's clean stop: `chronos mcp` (and `chronos serve`) close the database and
/// then exit the process, which runs no destructor, so the read sets kept only by dropping it were lost, and every
/// world that checks its reads was held after the restart as if after a crash. Here: a `chronos mcp` session that
/// ends with its input, then the world's reads, checked as before.
#[test]
fn reads_survive_a_server_closing_and_exiting() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let dir = std::env::temp_dir().join(format!("chronos-stale-exit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, "create table s (email text primary key); create world w with (check_reads = true)", &[])
            .unwrap();
        Session::new("w").run(&db, "select count(*) from s where email = 'a@x'", &[]).unwrap();
    } // dropped: kept
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["mcp", dir.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    mcp.stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}
"#,
        )
        .unwrap(); // input closed: it closes the database and exits
    assert!(mcp.wait().unwrap().success());
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.reads_changed("w").unwrap(), Vec::<(String, String)>::new(), "kept: nothing changed");
    run(&db, MAIN, "insert into s values ('a@x')", &[]).unwrap();
    let found = db.reads_changed("w").unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].1.contains("read rows that changed since it forked"), "{found:?}");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The row API's `get` (HTTP, MCP, the shell) is a read of its row like SQL's: recorded, and only the row, also for a
/// table without a SQL schema (telling that one exists reads none of its rows).
#[test]
fn the_row_apis_get_records_its_row_and_only_it() {
    let db = Db::new();
    db.put(MAIN, "notes/1", r#"{"v":1}"#).unwrap();
    db.put(MAIN, "notes/2", r#"{"v":2}"#).unwrap();
    run(&db, MAIN, "create world w with (check_reads = true)", &[]).unwrap();
    assert!(db.get("w", "notes/1").unwrap().is_some());
    // other rows of the table may change, and rows be added
    db.put(MAIN, "notes/2", r#"{"v":20}"#).unwrap();
    db.put(MAIN, "notes/3", r#"{"v":3}"#).unwrap();
    assert_eq!(db.reads_changed("w").unwrap(), vec![]);
    // the row it read may not
    db.put(MAIN, "notes/1", r#"{"v":10}"#).unwrap();
    let why = db.reads_changed("w").unwrap();
    assert!(why.len() == 1 && why[0].1.contains("notes/1"), "{why:?}");
    let e = run(&db, MAIN, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    // a row looked for and not found counts once it's added
    run(&db, MAIN, "create world gap with (check_reads = true)", &[]).unwrap();
    assert!(db.get("gap", "notes/9").unwrap().is_none());
    db.put(MAIN, "notes/9", r#"{"v":9}"#).unwrap();
    let why = db.reads_changed("gap").unwrap();
    assert!(why.iter().any(|(_, w)| w.contains("notes/9")), "{why:?}");
}
