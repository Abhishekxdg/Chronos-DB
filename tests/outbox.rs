//! Effects that happen only on merge: NOTIFY ON MERGE in a world queues an entry that nobody
//! hears until a merge brings it into main, where listeners hear it and it waits in the outbox
//! until a worker acknowledges it. Discarded or rolled back, it's never sent.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn ok(db: &Db, s: &mut Session, sql: &str) -> Vec<chronos::sql::Output> {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn heard(s: &Session) -> Vec<String> {
    s.inbox.take().into_iter().map(|n| format!("{}:{}", n.channel, n.payload)).collect()
}

fn outbox(db: &Db, world: &str) -> Vec<(String, String, String)> {
    db.outbox(world).unwrap().into_iter().map(|e| (e.id, e.payload, e.world)).collect()
}

#[test]
fn an_effect_happens_when_its_world_reaches_main() {
    let db = Db::new();
    let (worker, agent) = (&mut Session::new(MAIN), &mut Session::new(MAIN));
    ok(&db, worker, "listen billing");

    // queued in a world: nobody hears it, main's outbox doesn't have it
    ok(&db, agent, "create world fix; switch world fix; notify on merge billing, 'refund 42'");
    assert!(heard(worker).is_empty(), "not while the world is worked on");
    assert!(outbox(&db, MAIN).is_empty());
    let shown = ok(&db, agent, "show outbox").pop().unwrap();
    assert_eq!(shown.rows.len(), 1);
    assert_eq!((shown.rows[0][1].to_text(), shown.rows[0][3].to_text()), ("billing".into(), "fix".into()));

    // a dry run shows it among the rows
    let plan = db.merge_preview("fix", &Default::default()).unwrap();
    assert!(plan.rows.iter().any(|r| r.key.starts_with("_sonos_outbox/")));

    // merged: heard once, from the database (pid 0), and kept until acknowledged
    ok(&db, agent, "switch world main; merge world fix");
    let n = worker.inbox.take();
    assert_eq!((n.len(), n[0].pid, n[0].payload.as_str()), (1, 0, "refund 42"));
    let pending = outbox(&db, MAIN);
    assert_eq!((pending.len(), pending[0].1.as_str(), pending[0].2.as_str()), (1, "refund 42", "fix"));
    let id = &pending[0].0;
    assert_eq!(ok(&db, worker, &format!("ack outbox '{id}'"))[0].tag, "ACK 1");
    assert_eq!(ok(&db, worker, &format!("ack outbox '{id}', 'nosuch'"))[0].tag, "ACK 0", "twice is fine");
    assert!(outbox(&db, MAIN).is_empty());

    // discarded, or rolled back: never sent
    ok(&db, agent, "create world gone; switch world gone; notify on merge billing, 'never'; switch world main");
    ok(&db, agent, "drop world gone");
    ok(&db, agent, "create world txw; switch world txw; begin; notify on merge billing, 'undone'; rollback");
    assert!(outbox(&db, "txw").is_empty());
    ok(&db, agent, "begin; notify on merge billing, 'committed'; commit; switch world main; merge world txw");
    assert_eq!(heard(worker), ["billing:committed"]);

    // a merge into another world carries it along; only main sends it
    ok(&db, agent, "create world a; create world b from a; switch world b; notify on merge billing, 'nested'");
    ok(&db, agent, "switch world main; merge world b");
    assert!(heard(worker).is_empty(), "b merged into a, not main");
    assert_eq!(outbox(&db, "a").iter().filter(|e| e.1 == "nested").count(), 1);
    ok(&db, agent, "merge world a");
    assert_eq!(heard(worker), ["billing:nested"]);

    // on main: there already, heard as the statement (or its transaction) ends, once
    ok(&db, agent, "notify on merge billing, 'now'");
    assert_eq!(heard(worker), ["billing:now"]);
    ok(&db, agent, "begin; notify on merge billing, 'at commit'");
    assert!(heard(worker).is_empty());
    ok(&db, agent, "commit");
    assert_eq!(heard(worker), ["billing:at commit"]);
    assert_eq!(outbox(&db, MAIN).len(), 4, "committed, nested, now and at commit wait; the acknowledged one doesn't");
}

#[test]
fn agents_queue_effects_and_only_some_acknowledge_them() {
    let db = Db::new();
    run(&db, MAIN, "create table orders (id int primary key); create merge policy careful", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'careful')"), &[]).unwrap();
    run(&db, MAIN, "create agent reader with (can = 'read')", &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());

    // an effect isn't a schema change: a policy with schema = false merges it on its own
    ok(&db, &mut bot, "create world w; switch world w; insert into orders values (1); notify on merge orders, 'new 1'");
    ok(&db, &mut bot, "switch world main; merge world w");
    assert_eq!(outbox(&db, MAIN).len(), 1);
    // under a policy it changes main only by merging, effects too
    let e = bot.run(&db, "notify on merge orders, 'direct'", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);

    // anyone who reads main sees the outbox; working it needs 'outbox' (or admin): not
    // write_main, else an agent that may merge could take back the effects its merges queued
    let mut reader = Session::new(MAIN);
    reader.agent = Some("reader".into());
    assert_eq!(ok(&db, &mut reader, "show outbox")[0].rows.len(), 1);
    let id = outbox(&db, MAIN)[0].0.clone();
    for (who, s) in [("reader", &mut reader), ("bot", &mut bot)] {
        for q in [format!("ack outbox '{id}'"), format!("retry outbox '{id}' attempt 1"), "claim outbox".into()] {
            let e = s.run(&db, &q, &[]).unwrap_err();
            assert_eq!(e.code, "42501", "{who}: {q}: {}", e.message);
            assert!(e.message.contains("'outbox'"), "{}", e.message);
        }
    }
    run(&db, MAIN, "create agent worker with (can = 'read,outbox')", &[]).unwrap();
    let mut worker = Session::new(MAIN);
    worker.agent = Some("worker".into());
    assert_eq!(ok(&db, &mut worker, &format!("ack outbox '{id}'"))[0].tag, "ACK 1");
}

#[test]
fn merge_policies_count_queued_effects() {
    let db = Db::new();
    run(&db, MAIN, "create table orders (id int primary key)", &[]).unwrap();
    run(
        &db,
        MAIN,
        "create merge policy careful with (max_effects = 1, effects_per_hour = 2, review_channels = 'payments')",
        &[],
    )
    .unwrap();
    let shown = run(&db, MAIN, "show merge policies", &[]).unwrap().pop().unwrap();
    let col = |n: &str| shown.columns.as_ref().unwrap().iter().position(|(c, _)| c == n).unwrap();
    let p = &shown.rows[0];
    assert_eq!(
        (p[col("max_effects")].to_text(), p[col("effects_per_hour")].to_text(), p[col("review_channels")].to_text()),
        ("1".into(), "2".into(), "payments".into())
    );
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'careful')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    let merge = |bot: &mut Session, w: &str| {
        bot.run(&db, "use world main", &[]).unwrap();
        bot.run(&db, &format!("merge world {w}"), &[])
    };

    // one row changed, but a thousand emails: a person looks
    ok(&db, &mut bot, "create world many; use world many; insert into orders values (1)");
    ok(&db, &mut bot, "notify on merge email, 'a'; notify on merge email, 'b'");
    let e = merge(&mut bot, "many").unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("queues 2 effects (at most 1)") && e.message.contains("SHOW REVIEWS"), "{}", e.message);
    assert!(outbox(&db, MAIN).is_empty(), "nothing sent");

    // a channel that's always reviewed, even for one effect and no row
    ok(&db, &mut bot, "create world pay; use world pay; notify on merge payments, 'refund 9'");
    let e = merge(&mut bot, "pay").unwrap_err();
    assert!(e.message.contains("queues effects on payments (always reviewed)"), "{}", e.message);

    // the hour's: two merges of one effect each go through, the third waits
    for i in 0..3 {
        ok(&db, &mut bot, &format!("create world h{i}; use world h{i}; notify on merge email, 'h{i}'"));
        let r = merge(&mut bot, &format!("h{i}"));
        match i {
            0 | 1 => assert!(r.is_ok(), "{i}: {r:?}"),
            _ => {
                let e = r.unwrap_err();
                assert!(
                    e.message
                        .contains("queues 1 effects, and its merges queued 2 in the last hour (at most 2 an hour)"),
                    "{}",
                    e.message
                );
            }
        }
    }
    assert_eq!(outbox(&db, MAIN).len(), 2);
}

/// The worker's side: claimed, done, or failed; a lease that runs out on a worker that may have
/// called a receiver leaves the effect ambiguous, never sent again on its own.
#[test]
fn a_worker_that_dies_mid_call_leaves_the_effect_ambiguous_not_resent() {
    let dir = std::env::temp_dir().join(format!("chronos-outbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ids = {
        let db = Db::open(&dir).unwrap();
        let s = &mut Session::new(MAIN);
        ok(
            &db,
            s,
            "notify on merge webhook, 'charge 1'; notify on merge webhook, 'charge 2'; notify on merge email, 'hi'",
        );
        // two claims never get the same effect; LIMIT and FOR pick which
        let a = ok(&db, s, "claim outbox for webhook lease '200 milliseconds'").pop().unwrap();
        let b = ok(&db, s, "claim outbox for webhook limit 5 lease '200 milliseconds' idempotent").pop().unwrap();
        assert_eq!((a.rows.len(), b.rows.len()), (1, 1));
        assert_ne!(a.rows[0][0], b.rows[0][0]);
        let col = |n: &str| a.columns.as_ref().unwrap().iter().position(|(c, _)| c == n).unwrap();
        assert_eq!(
            (a.rows[0][col("state")].to_text(), a.rows[0][col("attempts")].to_text()),
            ("claimed".into(), "1".into())
        );
        assert!(ok(&db, s, "claim outbox for webhook").pop().unwrap().rows.is_empty(), "both are claimed");
        (a.rows[0][0].to_text(), b.rows[0][0].to_text())
        // the worker dies here: neither acknowledged
    };
    std::thread::sleep(std::time::Duration::from_millis(300)); // both leases run out
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    let state =
        |db: &Db, id: &str| db.outbox(MAIN).unwrap().into_iter().find(|e| e.id == id).map(|e| (e.state, e.attempts));
    // not idempotent: it may have reached the receiver, so it waits for someone to check
    assert_eq!(state(&db, &ids.0), Some(("ambiguous", 1)));
    // idempotent: the receiver drops a second call, so it may go again
    assert_eq!(state(&db, &ids.1), Some(("queued", 1)));
    let again = ok(&db, s, "claim outbox for webhook limit 10").pop().unwrap();
    assert_eq!(again.rows.len(), 1, "only the idempotent one comes back");
    assert_eq!(again.rows[0][0].to_text(), ids.1);
    assert_eq!(state(&db, &ids.1), Some(("claimed", 2)));
    assert_eq!(ok(&db, s, &format!("ack outbox '{}'", ids.1))[0].tag, "ACK 1");
    // someone checked the receiver: it never got charge 1, so it's queued again
    assert!(s.run(&db, &format!("retry outbox '{}'", ids.0), &[]).is_err(), "it names the attempt");
    assert_eq!(ok(&db, s, &format!("retry outbox '{}' attempt 2", ids.0))[0].tag, "RETRY 0", "not that attempt");
    assert_eq!(ok(&db, s, &format!("retry outbox '{}' attempt 1, 'nosuch' attempt 1", ids.0))[0].tag, "RETRY 1");
    assert_eq!(state(&db, &ids.0), Some(("queued", 1)));
    let c = ok(&db, s, "claim outbox for webhook").pop().unwrap();
    assert_eq!(c.rows[0][0].to_text(), ids.0);
    assert_eq!(state(&db, &ids.0), Some(("claimed", 2)));
    // the same retry again, late: the new claim stays its worker's
    assert_eq!(ok(&db, s, &format!("retry outbox '{}' attempt 1", ids.0))[0].tag, "RETRY 0");
    assert_eq!(state(&db, &ids.0), Some(("claimed", 2)));
    assert!(ok(&db, s, "claim outbox for webhook").pop().unwrap().rows.is_empty(), "never two workers at once");
    assert_eq!(ok(&db, s, &format!("ack outbox '{}'", ids.0))[0].tag, "ACK 1");
    // a queued effect isn't retried: there's nothing to undo
    let email = db.outbox(MAIN).unwrap()[0].id.clone();
    assert_eq!(ok(&db, s, &format!("retry outbox '{email}' attempt 0"))[0].tag, "RETRY 0");
    assert!(s.run(&db, "claim outbox lease '0 s'", &[]).is_err());
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
