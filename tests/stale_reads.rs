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

/// A held merge that turns out to be "nothing changed" costs a reviewer as much as a real one: a row changed since
/// only in columns no read used doesn't hold it. Issue #38.
#[test]
fn only_the_columns_a_read_used_hold_its_merge() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table orders (id int primary key, status text, amount int, notes text);
         insert into orders values (1, 'open', 10, ''), (2, 'open', 20, ''), (3, 'closed', 30, '')",
        &[],
    )
    .unwrap();
    // forks `w`, reads with `sql`, writes elsewhere, and main then runs `main`: the merge's error, if any
    let attempt = |w: &str, sql: &str, main: &str| {
        run(
            &db,
            MAIN,
            &format!("create table if not exists out_{w} (v int); create world {w} with (check_reads = true)"),
            &[],
        )
        .unwrap();
        run(&db, w, sql, &[]).unwrap();
        run(&db, w, &format!("insert into out_{w} values (1)"), &[]).unwrap();
        run(&db, MAIN, main, &[]).unwrap();
        run(&db, MAIN, &format!("merge world {w}"), &[]).err().map(|e| e.message)
    };
    let sum = "select sum(amount) from orders where status = 'open'";

    // a column the read never looked at
    assert_eq!(attempt("notes", sum, "update orders set notes = 'called' where id = 1"), None);
    // one it did: in its select list, its WHERE, its ORDER BY
    let e = attempt("amount", sum, "update orders set amount = 11 where id = 1").unwrap();
    assert!(e.contains("read rows that changed since it forked: orders/1"), "{e}");
    assert!(attempt("status", sum, "update orders set status = 'closed' where id = 2").is_some());
    let ordered = "select id from orders order by notes limit 1";
    assert!(attempt("ord", ordered, "update orders set notes = 'a' where id = 3").is_some());
    let distinct = "select distinct on (notes) id from orders order by notes, id";
    assert!(attempt("dist_on", distinct, "update orders set notes = 'g' where id = 2").is_some());
    // a read of rows alone (a count) isn't held by any update in place
    assert_eq!(attempt("counted", "select count(*) from orders", "update orders set amount = 99 where id = 3"), None);
    // a row added or removed changes what any read of the table finds
    assert!(attempt("added", sum, "insert into orders values (4, 'open', 5, '')").is_some());
    assert!(attempt("removed", "select count(*) from orders", "delete from orders where id = 3").is_some());

    // every column: *, a whole row, a key read outside SQL
    assert!(
        attempt("star", "select * from orders where id = 1", "update orders set notes = 'b' where id = 1").is_some()
    );
    assert!(
        attempt("whole", "select o from orders o where id = 1", "update orders set notes = 'c' where id = 1").is_some()
    );
    // a function made with CREATE FUNCTION may read anything
    run(
        &db,
        MAIN,
        "create function note_of(i int) returns text language sql as $$ select notes from orders where id = i $$",
        &[],
    )
    .unwrap();
    let f = "select note_of(1)";
    assert!(attempt("func", f, "update orders set notes = 'd' where id = 1").is_some());
    // and so may one a view calls: the view is looked into
    run(&db, MAIN, "create view order_notes as select id, note_of(id) as n from orders", &[]).unwrap();
    let v = "select id from order_notes where id = 1";
    assert!(attempt("viewfunc", v, "update orders set notes = 'f' where id = 1").is_some());
    // and so may an aggregate made with CREATE AGGREGATE, in place or as a window function
    run(
        &db,
        MAIN,
        "create function note_sum(acc text, i int) returns text language sql as $$ select coalesce(acc, '') || (select notes from orders where id = i) $$;
         create aggregate notes_of(int) (sfunc = note_sum, stype = text)",
        &[],
    )
    .unwrap();
    assert!(
        attempt("user_agg", "select notes_of(id) from orders", "update orders set notes = 'j' where id = 2").is_some()
    );
    let w = "select notes_of(id) over () from orders";
    assert!(attempt("user_win", w, "update orders set notes = 'k' where id = 1").is_some());
    // a bare name a subquery's own table hasn't is the outer query's
    run(
        &db,
        MAIN,
        "create table order_comments (order_id int, body text); insert into order_comments values (1, 'x'), (2, 'y')",
        &[],
    )
    .unwrap();
    let c = "select o.amount from orders o where exists (select 1 from order_comments n where n.order_id = o.id and status = 'open')";
    assert!(attempt("correlated", c, "update orders set status = 'held' where id = 1").is_some());
    // a function that reads the table itself (its size): every column
    let sized = "select amount, pg_table_size('orders') from orders limit 1";
    assert!(attempt("sized", sized, "update orders set notes = repeat('n', 9000) where id = 2").is_some());
    let d_sized =
        "select distinct on (id / pg_table_size('orders')) id from orders order by id / pg_table_size('orders'), id";
    assert!(attempt("dist_sized", d_sized, "update orders set notes = repeat('m', 9000) where id = 1").is_some());
    // a qualified name is the source that shows it: here `orders` names the second source, not the hidden table
    run(&db, MAIN, "create table audit (id int primary key, amount int); insert into audit values (1, 5)", &[])
        .unwrap();
    let hidden = "select orders.amount from orders as hidden cross join audit as orders";
    assert!(attempt("aliased", hidden, "update audit set amount = 6 where id = 1").is_some());
    let whole_hidden = "select orders from orders as hidden cross join audit as orders";
    assert!(attempt("aliased_row", whole_hidden, "update audit set amount = 7 where id = 1").is_some());
    // a function in DISTINCT ON, or a NATURAL JOIN's unnamed join columns: every column counts
    let d = "select distinct on (note_of(id)) id from orders order by note_of(id), id";
    assert!(attempt("dist_fn", d, "update orders set notes = 'h' where id = 1").is_some());
    run(&db, MAIN, "create table order_tags (id int primary key, notes text)", &[]).unwrap();
    let n = "select o.amount from orders o natural join order_tags";
    assert!(attempt("nat_join", n, "update orders set notes = 'i' where id = 2").is_some());

    // the columns of every read of a table count together
    run(&db, MAIN, "create world two with (check_reads = true)", &[]).unwrap();
    run(&db, "two", "select amount from orders where id = 1; select notes from orders where id = 2", &[]).unwrap();
    run(&db, "two", "insert into out_notes values (2)", &[]).unwrap();
    run(&db, MAIN, "update orders set notes = 'e' where id = 1", &[]).unwrap(); // notes, read only of row 2: counts
    assert!(run(&db, MAIN, "merge world two", &[]).is_err());

    // kept across a clean close and open
    let dir = std::env::temp_dir().join(format!("chronos-stale-columns-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, "create table t (id int primary key, a int, b int); insert into t values (1, 1, 1)", &[])
            .unwrap();
        run(&db, MAIN, "create table o (v int); create world w with (check_reads = true)", &[]).unwrap();
        run(&db, "w", "select a from t where id = 1; insert into o values (1)", &[]).unwrap();
    }
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "update t set b = 2 where id = 1", &[]).unwrap();
    run(&db, MAIN, "merge world w", &[]).unwrap();
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A world that runs its reads again (`rerun_reads`): a merge its reads would hold runs the SELECTs it read through
/// again, on main as it is and on its fork point, and is held only when one gives another answer now, saying what it
/// gave then and gives now. Rows read any other way hold it as before. Issue #38.
#[test]
fn a_hold_is_lifted_when_the_read_gives_the_same_answer() {
    use chronos::sql::Value;
    let db = Db::new();
    let fresh = |db: &Db| {
        run(
            db,
            MAIN,
            "create table if not exists orders (id int primary key, status text, amount int);
             create table if not exists out_rows (id int primary key, v int);
             delete from orders; delete from out_rows;
             insert into orders values (1, 'open', 10), (2, 'open', 20), (3, 'closed', 30)",
            &[],
        )
        .unwrap();
    };
    // forks `w` (with `with`), reads with `sql` (and `params`), writes elsewhere, and main then runs `main`: the
    // merge's error, if any
    let attempt = |w: &str, with: &str, sql: &str, params: &[Value], main: &str| {
        fresh(&db);
        run(&db, MAIN, &format!("create world {w} with ({with})"), &[]).unwrap();
        let mut s = Session::new(w);
        s.run(&db, sql, params).unwrap();
        s.run(&db, "insert into out_rows values (1, 1)", &[]).unwrap();
        run(&db, MAIN, main, &[]).unwrap();
        run(&db, MAIN, &format!("merge world {w}"), &[]).err().map(|e| e.message)
    };
    let sum = "select sum(amount) from orders where status = 'open'";
    let same_sum = "update orders set amount = 15 where id in (1, 2)"; // 10 + 20, then 15 + 15

    // the sum came out the same: held when only checked, not when run again
    let e = attempt("checked", "check_reads = true", sum, &[], same_sum).unwrap();
    assert!(e.contains("read rows that changed since it forked: orders/1, orders/2"), "{e}");
    assert_eq!(attempt("rerun", "rerun_reads = true", sum, &[], same_sum), None);
    assert_eq!(cell(&db, "select v from out_rows"), "1", "merged");
    // two changes that cancel out, through parameters
    let count = "select count(*) from orders where status = $1";
    let swap = "update orders set status = 'closed' where id = 1; update orders set status = 'open' where id = 3";
    assert_eq!(attempt("params", "rerun_reads = true", count, &[Value::Text("open".into())], swap), None);
    // the rows a filter found, found again (one of them changed, in the column filtered on)
    let over = "select id from orders where amount > 15";
    assert_eq!(attempt("found", "rerun_reads = true", over, &[], "update orders set amount = 40 where id = 3"), None);

    // another answer now: held, with what it gave then and gives now, and nothing merged
    let e = attempt("changed", "rerun_reads = true", sum, &[], "update orders set amount = 11 where id = 1").unwrap();
    assert!(e.starts_with("merging changed read rows that changed since it forked: orders/1"), "{e}");
    assert!(
        e.contains("read `select sum(amount) from orders where status = 'open'`, which gave 30 then and gives 31 now"),
        "{e}"
    );
    assert!(db.world("changed").is_ok() && cell(&db, "select count(*) from out_rows") == "0");
    // CHECK READS between steps says the same, without merging
    let found = db.reads_changed("changed").unwrap();
    assert!(found.iter().any(|(_, r)| r.contains("which gave 30 then and gives 31 now")), "{found:?}");
    let e = attempt(
        "rows",
        "rerun_reads = true",
        "select id from orders where status = 'open' order by id",
        &[],
        "insert into orders values (4, 'open', 1)",
    )
    .unwrap();
    assert!(e.contains("which gave 2 rows then and gives 3 rows now"), "{e}");

    // rows read another way (here a row the row API got) hold it as found, whatever the SELECTs give
    fresh(&db);
    db.put(MAIN, "notes/1", r#"{"v":1}"#).unwrap();
    run(&db, MAIN, "create world mixed with (rerun_reads = true)", &[]).unwrap();
    run(&db, "mixed", sum, &[]).unwrap();
    assert!(db.get("mixed", "notes/1").unwrap().is_some());
    run(&db, "mixed", "insert into out_rows values (1, 1)", &[]).unwrap();
    run(&db, MAIN, same_sum, &[]).unwrap();
    db.put(MAIN, "notes/1", r#"{"v":2}"#).unwrap();
    let e = run(&db, MAIN, "merge world mixed", &[]).unwrap_err().message;
    assert!(e.contains("notes/1") && !e.contains("which gave"), "{e}");
    // a read of the world's own writes gives another answer on the fork point: held as found
    fresh(&db);
    run(&db, MAIN, "create world own with (rerun_reads = true)", &[]).unwrap();
    run(&db, "own", "insert into orders values (4, 'open', 5)", &[]).unwrap();
    run(&db, "own", sum, &[]).unwrap();
    run(&db, MAIN, same_sum, &[]).unwrap();
    let e = run(&db, MAIN, "merge world own", &[]).unwrap_err().message;
    assert!(e.contains("orders/1") && !e.contains("which gave"), "{e}");
    // and one of the clock: what it gives depends on when it runs
    fresh(&db);
    run(&db, MAIN, "create world clock with (rerun_reads = true)", &[]).unwrap();
    run(&db, "clock", "select sum(amount), clock_timestamp() from orders where status = 'open'", &[]).unwrap();
    run(&db, "clock", "insert into out_rows values (1, 1)", &[]).unwrap();
    run(&db, MAIN, same_sum, &[]).unwrap();
    assert!(run(&db, MAIN, "merge world clock", &[]).is_err());
    // a SELECT that failed read and isn't kept: held as found
    fresh(&db);
    run(&db, MAIN, "create world failed with (rerun_reads = true)", &[]).unwrap();
    run(&db, "failed", sum, &[]).unwrap();
    assert!(run(&db, "failed", "select amount / (amount - 10) from orders where status = 'open'", &[]).is_err());
    run(&db, "failed", "insert into out_rows values (1, 1)", &[]).unwrap();
    run(&db, MAIN, same_sum, &[]).unwrap();
    assert!(run(&db, MAIN, "merge world failed", &[]).is_err());

    // an agent whose merge policy has rerun_reads: its worlds check their reads and run them again
    fresh(&db);
    run(&db, MAIN, "create merge policy rerun with (rerun_reads = true)", &[]).unwrap();
    let shown = run(&db, MAIN, "show merge policies", &[]).unwrap().pop().unwrap();
    let at = shown.columns.as_ref().unwrap().iter().position(|(n, _)| n == "rerun_reads").unwrap();
    assert_eq!(shown.rows[0][at].to_text(), "true");
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'rerun')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(&db, &format!("create world pay; use world pay; {sum}; insert into out_rows values (1, 1)"), &[]).unwrap();
    run(&db, MAIN, same_sum, &[]).unwrap();
    bot.run(&db, "use world main; merge world pay", &[]).unwrap();
    assert_eq!(cell(&db, "select v from out_rows"), "1");
    bot.run(&db, &format!("create world pay2; use world pay2; {sum}; insert into out_rows values (2, 2)"), &[])
        .unwrap();
    run(&db, MAIN, "update orders set amount = 1 where id = 1", &[]).unwrap();
    let e = bot.run(&db, "use world main; merge world pay2", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "queued for a person: {}", e.message);
    let reviews = run(&db, MAIN, "show reviews", &[]).unwrap().pop().unwrap();
    let why = format!("{:?}", reviews.rows);
    assert!(why.contains("which gave 30 then and gives 16 now"), "{why}");

    // what was kept isn't written out: after a restart a hold stays as found
    let dir = std::env::temp_dir().join(format!("chronos-stale-rerun-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        fresh(&db);
        run(&db, MAIN, "create world w with (rerun_reads = true)", &[]).unwrap();
        run(&db, "w", sum, &[]).unwrap();
        run(&db, "w", "insert into out_rows values (1, 1)", &[]).unwrap();
    }
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, same_sum, &[]).unwrap();
    assert!(run(&db, MAIN, "merge world w", &[]).is_err());
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An agent held to a tenant reads only its tenant's rows, of child tables through their foreign keys too: of a table
/// it read whole, another tenant's rows may change without holding its merge; a row of its own, or one moved into its
/// tenant, holds it. A read made in its world for no tenant (a person's) read every row: any change holds that.
#[test]
fn a_tenants_reads_are_held_only_by_its_tenants_rows() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table leads (id int primary key, org_id int not null, stage text);
         create table notes (id int primary key, lead_id int references leads (id), body text);
         create table tasks (id int primary key, note_id int references notes (id), done bool);
         insert into leads values (1, 42, 'new'), (2, 42, 'new'), (3, 7, 'new'), (4, 7, 'new');
         insert into notes values (10, 1, 'a'), (11, 2, 'b'), (12, 3, 'c'), (13, 4, 'd');
         insert into tasks values (100, 10, false), (101, 11, false), (102, 12, false), (103, 13, false)",
        &[],
    )
    .unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent run42 with (can = '{can}', tenant = 'org_id = 42')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("run42".into());
    // reads notes and tasks whole (through its tenant's leads), and changes its own
    let mut work = |w: &str| {
        let sql = format!(
            "create world {w} with (check_reads = true); use world {w};
             select count(*) from notes; select count(*) from tasks where not done;
             update leads set stage = 'called' where id = 1; use world main"
        );
        bot.run(&db, &sql, &[]).unwrap();
    };
    let merge = |w: &str| {
        let mut bot = Session::new(MAIN);
        bot.agent = Some("run42".into());
        bot.run(&db, &format!("merge world {w}"), &[])
    };
    // (the row, or its table when the read covered the whole of it)
    let held = |w: &str, row: &str| {
        let e = merge(w).unwrap_err();
        assert_eq!(e.code, "40001", "{}", e.message);
        let table = row.split('/').next().unwrap();
        assert!(
            e.message.contains(&format!("changed since it forked: {row}"))
                || e.message.contains(&format!("read {table}, which changed")),
            "{row}: {}",
            e.message
        );
        run(&db, MAIN, &format!("drop world {w}"), &[]).unwrap();
    };

    // another tenant's lead, notes and tasks change, and it gets a new note: not held
    work("others");
    run(
        &db,
        MAIN,
        "update leads set stage = 'won' where id = 3; update notes set body = 'x' where id = 12;
         update tasks set done = true where id = 103; insert into notes values (14, 4, 'e')",
        &[],
    )
    .unwrap();
    merge("others").unwrap();
    assert_eq!(cell(&db, "select stage from leads where id = 1"), "called");

    // a note of its own changes, a task of its own is added, a note moves under its lead: each holds
    work("own");
    run(&db, MAIN, "update notes set body = 'x' where id = 11", &[]).unwrap();
    held("own", "notes/11");
    work("added");
    run(&db, MAIN, "insert into tasks values (104, 10, false)", &[]).unwrap();
    held("added", "tasks/104");
    work("moved");
    run(&db, MAIN, "update notes set lead_id = 2 where id = 13", &[]).unwrap();
    held("moved", "notes/13");

    // a person read notes in its world too: every row counts
    work("person");
    run(&db, "person", "select count(*) from notes", &[]).unwrap();
    run(&db, MAIN, "update notes set body = 'y' where id = 12", &[]).unwrap();
    held("person", "notes/12");
}

fn cell(db: &Db, sql: &str) -> String {
    run(db, MAIN, sql, &[]).unwrap().pop().unwrap().rows[0][0].to_text()
}
