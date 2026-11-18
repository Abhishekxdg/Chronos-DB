//! Evidence attached to a world (ATTACH EVIDENCE: a ticket, an order snapshot, ...), kept in its metadata, fixed
//! once its merge is proposed, and asked for by merge policies (`require_evidence`).

use chronos::sql::{Output, Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};

const WRITER: [Can; 5] = [Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain, Can::MergeOwn];

fn agent(db: &Db, name: &str, can: &[Can], policy: Option<&str>) -> Session {
    let spec = AgentSpec { can: Some(can.to_vec()), policy: policy.map(String::from), ..Default::default() };
    db.create_agent(name, &spec).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some(name.into());
    s
}

fn col(out: &[Output], col: &str) -> String {
    let o = out.last().unwrap();
    let cols = o.columns.as_ref().unwrap();
    let at = cols.iter().position(|(c, _)| c == col).unwrap();
    o.rows[0][at].to_text()
}

fn setup(db: &Db) {
    run(db, MAIN, "create table t (id int primary key, v text); insert into t values (1, 'a'), (2, 'b')", &[]).unwrap();
}

const TICKET: &str = "(kind = 'ticket', uri = 'https://tickets.example/42', sha256 = \
                      'aa11bb22cc33dd44ee55ff6600112233445566778899aabbccddeeff00112233', note = 'customer asked')";

#[test]
fn require_evidence_holds_without_ticket() {
    let db = Db::new();
    setup(&db);
    // a setting that isn't a list of kinds is refused
    assert_eq!(run(&db, MAIN, "create merge policy bad with (require_evidence = 5)", &[]).unwrap_err().code, "22023");
    run(&db, MAIN, "create merge policy ticketed with (require_evidence = 'ticket')", &[]).unwrap();
    let shown = run(&db, MAIN, "show merge policies", &[]).unwrap();
    assert!(format!("{:?}", shown[0].rows).contains("ticket"), "{:?}", shown[0].rows);
    let mut bot = agent(&db, "bot", &WRITER, Some("ticketed"));
    bot.run(&db, "create world w; use world w; update t set v = 'refunded' where id = 1; use world main", &[]).unwrap();
    let e = bot.run(&db, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("evidence"), "{}", e.message);
    assert!(e.message.contains("ticket"), "{}", e.message);
    assert_eq!(run(&db, MAIN, "select v from t where id = 1", &[]).unwrap()[0].rows[0][0].to_text(), "a");

    // with a ticket attached it goes through on its own
    let out = bot.run(&db, &format!("attach evidence to world w {TICKET}"), &[]).unwrap();
    assert_eq!(out[0].tag, "ATTACH EVIDENCE");
    bot.run(&db, "merge world w", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "select v from t where id = 1", &[]).unwrap()[0].rows[0][0].to_text(), "refunded");

    // evidence of another kind doesn't count
    bot.run(&db, "create world x; use world x; update t set v = 'x' where id = 2; use world main", &[]).unwrap();
    bot.run(&db, "attach evidence to world x (kind = 'screenshot', uri = 'file:///a.png')", &[]).unwrap();
    let e = bot.run(&db, "merge world x", &[]).unwrap_err();
    assert!(e.message.contains("evidence"), "{}", e.message);
}

#[test]
fn evidence_immutable_after_propose() {
    let db = Db::new();
    setup(&db);
    let mut bot = agent(&db, "bot", &WRITER, None);
    let mut other = agent(&db, "other", &WRITER, None);
    bot.run(&db, "create world w; use world w; insert into t values (3, 'c'); use world main", &[]).unwrap();
    // a kind is required, and only known settings
    assert_eq!(bot.run(&db, "attach evidence to world w (uri = 'x')", &[]).unwrap_err().code, "22023");
    assert_eq!(bot.run(&db, "attach evidence to world w (kind = 'a', color = 'red')", &[]).unwrap_err().code, "22023");
    // only who may change the world attaches to it
    assert_eq!(other.run(&db, &format!("attach evidence to world w {TICKET}"), &[]).unwrap_err().code, "42501");
    // nor may anyone set the database's key for it by hand
    assert!(run(&db, MAIN, "alter world w set (_sonos_evidence = 'forged')", &[]).is_err());

    bot.run(&db, &format!("attach evidence to world w {TICKET}"), &[]).unwrap();
    let w = db.world("w").unwrap();
    let ev = w.meta["_sonos_evidence"].as_array().cloned().unwrap_or_default();
    assert_eq!(ev.len(), 1, "{:?}", w.meta);
    assert_eq!(ev[0]["kind"], "ticket");
    assert_eq!(ev[0]["by"], "bot");
    assert!(ev[0]["id"].is_string() && ev[0]["at"].is_u64(), "{ev:?}");

    // proposed: fixed
    let id = col(&bot.run(&db, "propose merge world w", &[]).unwrap(), "id");
    let e = bot.run(&db, "attach evidence to world w (kind = 'note')", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    // a person can't change it either while it's open
    assert_eq!(run(&db, MAIN, "attach evidence to world w (kind = 'note')", &[]).unwrap_err().code, "55000");
    // withdrawn: open to more
    bot.run(&db, &format!("withdraw proposal '{id}'"), &[]).unwrap();
    bot.run(&db, "attach evidence to world w (kind = 'note')", &[]).unwrap();
    assert_eq!(db.world("w").unwrap().meta["_sonos_evidence"].as_array().unwrap().len(), 2);

    // merged into another world (the world stays): still fixed
    run(&db, MAIN, "create world dest", &[]).unwrap();
    let id = col(&run(&db, MAIN, "propose merge world w into dest", &[]).unwrap(), "id");
    run(&db, MAIN, &format!("approve proposal '{id}'"), &[]).unwrap();
    run(&db, MAIN, &format!("merge proposal '{id}'"), &[]).unwrap();
    assert!(db.world("w").is_ok(), "merged INTO keeps the world");
    let e = run(&db, MAIN, "attach evidence to world w (kind = 'late')", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
}
