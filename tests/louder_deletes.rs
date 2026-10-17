//! Destructive changes are louder than additive ones wherever a person reviews a world (issue #27): `DIFF WORLD`,
//! `MERGE ... DRY RUN` and `SHOW REVIEWS` lead with what each table loses, deletes before the rest; and a merge
//! policy's `confirm_deletes_over` has the person approving a big delete give its count, counted by the merge
//! itself, so approving means having read the number.

use chronos::sql::{Output, Session, run};
use chronos::{Db, MAIN};

fn q(db: &Db, w: &str, sql: &str) -> Output {
    run(db, w, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap()
}

fn col(o: &Output, name: &str) -> usize {
    o.columns.as_ref().unwrap().iter().position(|(c, _)| c == name).unwrap_or_else(|| panic!("no column {name}"))
}

#[test]
fn deletes_lead_and_big_ones_need_the_count() {
    let db = Db::new();
    q(&db, MAIN, "create table leads (id int primary key, owner text)");
    q(&db, MAIN, "insert into leads select g, 'ana' from generate_series(1, 200) g");
    q(&db, MAIN, "create table notes (id int primary key, body text)");
    q(&db, MAIN, "insert into notes values (1, 'a'), (2, 'b')");
    q(&db, MAIN, "create merge policy careful with (confirm_deletes_over = 100)");
    let can = "read,fork,write_own,merge_own,write_main";
    q(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'careful')"));
    let shown = q(&db, MAIN, "show merge policies");
    assert_eq!(shown.rows[0][col(&shown, "confirm_deletes_over")].to_text(), "100");

    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(
        &db,
        "create world wipe; use world wipe; delete from leads where id > 50; \
         update notes set body = 'z' where id = 1; insert into notes values (3, 'c'); use world main",
        &[],
    )
    .unwrap();
    // the agent's own merge goes to a person: it can't give the count for itself
    let e = bot.run(&db, "merge world wipe", &[]).unwrap_err();
    assert!(e.message.contains("deletes 150 rows (over 100") && e.message.contains("SHOW REVIEWS"), "{}", e.message);
    let e = bot.run(&db, "merge world wipe confirm deletes 150", &[]).unwrap_err();
    assert!(e.message.contains("SHOW REVIEWS"), "an agent's count doesn't count: {}", e.message);

    // the queue leads with what each table loses, the table that deletes most first
    let r = q(&db, MAIN, "show reviews");
    let wipe = r.rows.iter().find(|row| row[0].to_text() == "wipe").unwrap();
    assert_eq!(
        wipe[col(&r, "changes")].to_text(),
        "leads: 150 deleted, 0 updated, 0 inserted; notes: 0 deleted, 1 updated, 1 inserted"
    );

    // DIFF WORLD: the deletes before the rest
    let d = q(&db, MAIN, "diff world wipe");
    let change = col(&d, "change");
    let kinds: Vec<String> = d.rows.iter().map(|r| r[change].to_text()).collect();
    assert_eq!(kinds.len(), 152);
    assert!(kinds[..150].iter().all(|k| k == "delete"), "{kinds:?}");
    assert!(kinds[150..].iter().all(|k| k != "delete"), "{kinds:?}");

    // the dry run: a summary line per table, then the rows it deletes, said so, then the rest
    let p = q(&db, MAIN, "merge world wipe dry run");
    let (table, outcome, detail) = (col(&p, "table"), col(&p, "outcome"), col(&p, "detail"));
    let line = |i: usize| (p.rows[i][table].to_text(), p.rows[i][outcome].to_text(), p.rows[i][detail].to_text());
    assert_eq!(line(0), ("leads".into(), "summary".into(), "150 deleted, 0 updated, 0 inserted".into()));
    assert_eq!(line(1), ("notes".into(), "summary".into(), "0 deleted, 1 updated, 1 inserted".into()));
    for i in 2..152 {
        assert_eq!(line(i).2, "DELETE: the merge deletes this row", "row {i}");
    }
    assert!((152..154).all(|i| line(i).1 == "apply" && !line(i).2.contains("DELETE")));
    let plan = db.merge_preview("wipe", &Default::default()).unwrap();
    let c = plan.confirm_deletes.expect("the count is asked for");
    assert_eq!((c.deletes, c.over, c.policy.as_str()), (150, 100, "careful"));

    // a person approving it: no count, or a wrong one, is refused with the real count; nothing merges
    for (sql, said) in [
        ("merge world wipe", "it deletes 150 rows"),
        ("merge world wipe confirm deletes 149", "CONFIRM DELETES 149 doesn't match: it deletes 150"),
        // plain CONFIRM is the crash confirm, not this one
        ("merge world wipe confirm", "it deletes 150 rows"),
    ] {
        let e = run(&db, MAIN, sql, &[]).unwrap_err();
        assert_eq!(e.code, "55000", "{sql}: {}", e.message);
        assert!(e.message.contains(said) && e.message.contains("CONFIRM DELETES 150"), "{sql}: {}", e.message);
        assert_eq!(q(&db, MAIN, "select count(*) from leads").rows[0][0].to_text(), "200", "{sql}");
    }
    // a count that isn't a number
    assert!(run(&db, MAIN, "merge world wipe confirm deletes many", &[]).is_err());
    // the right one merges
    q(&db, MAIN, "merge world wipe confirm deletes 150");
    assert_eq!(q(&db, MAIN, "select count(*) from leads").rows[0][0].to_text(), "50");

    // at or under the threshold: no count asked
    bot.run(&db, "create world few; use world few; delete from leads where id <= 5; use world main", &[]).unwrap();
    assert!(db.merge_preview("few", &Default::default()).unwrap().confirm_deletes.is_none());
    q(&db, MAIN, "merge world few");
    assert_eq!(q(&db, MAIN, "select count(*) from leads").rows[0][0].to_text(), "45");

    // a world of no agent with such a policy: as before
    q(&db, MAIN, "create world mine");
    q(&db, "mine", "delete from leads");
    q(&db, MAIN, "merge world mine");
}
