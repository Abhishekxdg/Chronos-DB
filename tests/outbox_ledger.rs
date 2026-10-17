//! Where an effect came from and where it went (issues #30 and #31): each effect that lands in main is tied to the
//! merge that brought it and to who let it through, and keeps its delivery state per merge after it's
//! acknowledged; an effect discarded with its world is recorded (channel, payload hash, why), never sent; an effect
//! that turns ambiguous is announced on a channel of its own; a review lists what a world's merge would send.

use chronos::sql::{Output, Session, run};
use chronos::{Db, MAIN};

fn q(db: &Db, w: &str, sql: &str) -> Output {
    run(db, w, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap()
}

fn col(o: &Output, name: &str) -> usize {
    o.columns.as_ref().unwrap().iter().position(|(c, _)| c == name).unwrap_or_else(|| panic!("no column {name}"))
}

fn cell(o: &Output, row: usize, name: &str) -> String {
    o.rows[row][col(o, name)].to_text()
}

fn agent(name: &str) -> Session {
    let mut s = Session::new(MAIN);
    s.agent = Some(name.into());
    s
}

#[test]
fn effects_are_traced_to_their_merge_and_kept_when_vetoed() {
    let dir = std::env::temp_dir().join(format!("chronos-outbox-ledger-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    q(&db, MAIN, "create table t (id int primary key)");
    q(&db, MAIN, "create merge policy loose with (max_rows = 100)");
    q(&db, MAIN, "create merge policy strict with (max_rows = 0)");
    let can = "read,fork,write_own,merge_own,write_main";
    q(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'loose')"));
    q(&db, MAIN, &format!("create agent careful with (can = '{can}', policy = 'strict')"));

    // merged on its policy's rules: the outbox says from where, and how it was let through
    let mut bot = agent("bot");
    bot.run(
        &db,
        "create world w1; use world w1; insert into t values (1); notify on merge email, 'hi ada'; use world main",
        &[],
    )
    .unwrap();
    assert_eq!(db.world_effects("w1").unwrap().len(), 1, "a review lists what its merge would send");
    bot.run(&db, "merge world w1", &[]).unwrap();
    let o = q(&db, MAIN, "show outbox");
    assert_eq!(o.rows.len(), 1);
    assert_eq!(cell(&o, 0, "merged_from"), "w1");
    assert_eq!(cell(&o, 0, "approved"), "on merge policy loose's rules");
    let id = cell(&o, 0, "id");
    let e = q(&db, MAIN, "show merge w1 effects");
    assert_eq!(
        (cell(&e, 0, "id"), cell(&e, 0, "state"), cell(&e, 0, "merged_by")),
        (id.clone(), "queued".into(), "bot".into())
    );
    // a worker sent it: still there per merge, done
    q(&db, MAIN, &format!("ack outbox '{id}'"));
    assert_eq!(cell(&q(&db, MAIN, "show merge w1 effects"), 0, "state"), "done");

    // held for review, then rejected: recorded as discarded, never sent, its payload only as a hash
    let mut careful = agent("careful");
    careful
        .run(&db, "create world w2; use world w2; insert into t values (2); notify on merge webhook, 'charge 5'; use world main", &[])
        .unwrap();
    assert!(careful.run(&db, "merge world w2", &[]).unwrap_err().message.contains("SHOW REVIEWS"));
    let r = q(&db, MAIN, "show reviews");
    assert_eq!(cell(&r, 0, "effects"), "webhook (1)", "the review shows what approving it would send");
    q(&db, MAIN, "drop world w2");
    let d = q(&db, MAIN, "show outbox discarded");
    assert_eq!(d.rows.len(), 1);
    assert_eq!(
        (cell(&d, 0, "channel"), cell(&d, 0, "world"), cell(&d, 0, "queued_by")),
        ("webhook".into(), "w2".into(), "careful".into())
    );
    assert_eq!(d.rows[0][col(&d, "payload")], chronos::sql::Value::Null, "no payload unless asked");
    assert_eq!(cell(&d, 0, "payload_sha256"), "7a150bcffbb22503cadfd9e774a903658d111910e2e004ab125b3fd419645fb1");
    let why = cell(&d, 0, "why");
    assert!(why.contains("rejected") && why.contains("strict"), "{why}");
    assert!(db.outbox(MAIN).unwrap().is_empty(), "never sent");

    // a person approving a held merge
    careful
        .run(
            &db,
            "create world w3; use world w3; insert into t values (3); notify on merge sms, 'hello'; use world main",
            &[],
        )
        .unwrap();
    assert!(careful.run(&db, "merge world w3", &[]).is_err());
    q(&db, MAIN, "merge world w3");
    let e = q(&db, MAIN, "show merge w3 effects");
    assert_eq!(cell(&e, 0, "approved"), "approved by a person after review (merge policy strict)");
    assert_eq!(e.rows[0][col(&e, "merged_by")], chronos::sql::Value::Null, "a person");

    // payloads kept once asked for
    q(&db, MAIN, "alter system set outbox_keep_payloads = on");
    assert_eq!(q(&db, MAIN, "show outbox_keep_payloads").rows[0][0].to_text(), "on");
    q(&db, MAIN, "create world w4");
    q(&db, "w4", "notify on merge email, 'kept'");
    q(&db, MAIN, "drop world w4");
    let d = q(&db, MAIN, "show outbox discarded since '-1 hour'");
    assert_eq!(d.rows.len(), 2);
    assert_eq!(cell(&d, 1, "payload"), "kept");
    assert!(cell(&d, 1, "why").contains("dropped by a person"), "{}", cell(&d, 1, "why"));

    // the merge outcomes count what each agent had vetoed
    let m = q(&db, MAIN, "show merge outcomes");
    let row = m.rows.iter().position(|r| r[col(&m, "agent")].to_text() == "careful").expect("a row for careful");
    assert_eq!(cell(&m, row, "effects_discarded"), "1");

    // kept across a restart
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(q(&db, MAIN, "show outbox discarded").rows.len(), 2);
    assert_eq!(cell(&q(&db, MAIN, "show merge w1 effects"), 0, "state"), "done");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_effect_turning_ambiguous_is_announced_once() {
    let db = Db::new();
    let mut ops = Session::new(MAIN);
    ops.run(&db, "listen chronos_outbox_ambiguous", &[]).unwrap();
    q(&db, MAIN, "create world w");
    q(&db, "w", "notify on merge webhook, 'charge 5'");
    q(&db, MAIN, "merge world w");
    q(&db, MAIN, "claim outbox for webhook lease '100 milliseconds'");
    std::thread::sleep(std::time::Duration::from_millis(250)); // the worker died mid-call
    let o = q(&db, MAIN, "show outbox");
    assert_eq!(cell(&o, 0, "state"), "ambiguous");
    let heard = ops.inbox.take();
    assert_eq!(heard.len(), 1, "{heard:?}");
    let p: serde_json::Value = serde_json::from_str(&heard[0].payload).unwrap();
    assert_eq!(
        (p["channel"].as_str(), p["attempt"].as_u64(), p["world"].as_str()),
        (Some("webhook"), Some(1), Some("w"))
    );
    q(&db, MAIN, "show outbox");
    assert!(ops.inbox.take().is_empty(), "said once");
}
