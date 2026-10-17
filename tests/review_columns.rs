//! Merge policies' `review_columns`: a change to a listed column waits for a person, even in one
//! row. An update that changes it, an insert that gives it a value and a delete that takes one
//! count; other columns of the same table don't.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn setup() -> (Db, Session) {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table leads (id int primary key, status text, owner text);
         create table \"Accounts\" (id int primary key, \"Owner\" text);
         insert into leads values (1, 'open', 'ada'), (2, 'open', 'bo'), (3, 'open', null);
         insert into \"Accounts\" values (1, 'ada');
         create merge policy careful with (review_columns = 'leads.owner, \"Accounts\".\"Owner\"')",
        &[],
    )
    .unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'careful')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    (db, bot)
}

/// As the agent: forks `w`, runs `sql` in it and merges it; the merge's error, if any.
fn attempt(db: &Db, bot: &mut Session, w: &str, sql: &str) -> Option<String> {
    bot.run(db, &format!("create world {w}; use world {w}; {sql}; use world main"), &[]).unwrap();
    match bot.run(db, &format!("merge world {w}"), &[]) {
        Ok(_) => None,
        Err(e) => {
            assert_eq!(e.code, "42501", "{}", e.message);
            assert!(e.message.contains("SHOW REVIEWS"), "{}", e.message);
            Some(e.message)
        }
    }
}

#[test]
fn a_change_to_a_listed_column_waits_for_a_person() {
    let (db, mut bot) = setup();
    // shown as the setting takes it back
    let shown = run(&db, MAIN, "show merge policies", &[]).unwrap().pop().unwrap();
    let col = shown.columns.as_ref().unwrap().iter().position(|(n, _)| n == "review_columns").unwrap();
    assert_eq!(shown.rows[0][col].to_text(), r#"leads.owner,"Accounts"."Owner""#);
    assert_eq!(db.policy("careful").unwrap().review_columns.len(), 2);

    // other columns merge on their own
    assert_eq!(attempt(&db, &mut bot, "status", "update leads set status = 'won' where id = 1"), None);
    assert_eq!(attempt(&db, &mut bot, "blank", "insert into leads values (4, 'open', null)"), None);

    // one row is enough
    let e = attempt(&db, &mut bot, "owner", "update leads set owner = 'bot' where id = 2").unwrap();
    assert!(e.contains("changes leads.owner (always reviewed): leads/2"), "{e}");
    let e = attempt(&db, &mut bot, "given", "insert into leads values (5, 'open', 'cy')").unwrap();
    assert!(e.contains("leads.owner (always reviewed): leads/5"), "{e}");
    let e = attempt(&db, &mut bot, "gone", "delete from leads where id = 1").unwrap();
    assert!(e.contains("leads.owner (always reviewed): leads/1"), "{e}");
    let e = attempt(&db, &mut bot, "quoted", "update \"Accounts\" set \"Owner\" = 'bot'").unwrap();
    assert!(e.contains("changes Accounts.Owner (always reviewed)"), "{e}");
    assert_eq!(db.reviews().len(), 4);

    // a row without a value there, deleted: nothing of the column changes
    assert_eq!(attempt(&db, &mut bot, "empty", "delete from leads where id = 3"), None);

    // the parent's own change to the column, combined in by columns, isn't the world's
    bot.run(
        &db,
        "create world side; use world side; update leads set status = 'lost' where id = 2; use world main",
        &[],
    )
    .unwrap();
    run(&db, MAIN, "update leads set owner = 'dee' where id = 2", &[]).unwrap();
    bot.run(&db, "merge world side by columns", &[]).unwrap();

    // a person merges what's queued
    run(&db, MAIN, "merge world owner", &[]).unwrap_err(); // leads/2's owner changed on main since: a conflict
    run(&db, MAIN, "merge world given", &[]).unwrap();
    assert_eq!(db.reviews().len(), 3);
}

#[test]
fn review_columns_must_name_columns() {
    let (db, _) = setup();
    let e = run(&db, MAIN, "alter merge policy careful set (review_columns = 'leads')", &[]).unwrap_err();
    assert!(e.message.contains("review_columns must be a list of columns like"), "{}", e.message);
    assert!(run(&db, MAIN, "alter merge policy careful set (review_columns = 'leads.\"owner')", &[]).is_err());
    for bad in [".owner", "leads.", "leads..owner"] {
        let sql = format!("alter merge policy careful set (review_columns = '{bad}')");
        assert!(run(&db, MAIN, &sql, &[]).is_err(), "{bad} names no column");
    }
    run(&db, MAIN, "alter merge policy careful set (review_columns = null)", &[]).unwrap();
    assert!(db.policy("careful").unwrap().review_columns.is_empty());
}
