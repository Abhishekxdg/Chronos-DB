//! Decision intelligence: EXPLAIN MERGE WORLD (everything a merge would do and why, in one answer), SHOW DECISION
//! PROVENANCE (how a merge came to be decided), tombstones of worlds that didn't make it (SHOW TOMBSTONES), and the
//! certificate covering rows a diff doesn't show.

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

/// The rows of the last output as (first column, the rest joined by ' | ').
fn lines(out: &[Output]) -> Vec<(String, String)> {
    out.last()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r[0].to_text(), r[1..].iter().map(|v| v.to_text()).collect::<Vec<_>>().join(" | ")))
        .collect()
}

fn has(lines: &[(String, String)], section: &str, text: &str) -> bool {
    lines.iter().any(|(s, d)| s == section && d.contains(text))
}

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create table t (id int primary key, v text, amount int);
         insert into t select g, 'v' || g, g * 10 from generate_series(1, 5) g",
        &[],
    )
    .unwrap();
}

fn certificate(db: &Db, w: &str) -> String {
    let out = run(db, MAIN, &format!("explain merge world {w}"), &[]).unwrap();
    let l = lines(&out);
    let c = l.iter().find(|(s, _)| s == "certificate").unwrap_or_else(|| panic!("{l:?}"));
    c.1.split(" | ").next().unwrap().to_string()
}

#[test]
fn explain_merge_lists_readers_policy_effects() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "register reader nightly_export on t (v)", &[]).unwrap();
    run(&db, MAIN, "create merge policy small with (max_rows = 1)", &[]).unwrap();
    let mut bot = agent(&db, "bot", &WRITER, Some("small"));
    bot.run(
        &db,
        "create world w; use world w; update t set v = 'x' where id in (1, 2);
         notify on merge billing, 'refund 42'; use world main",
        &[],
    )
    .unwrap();
    bot.run(&db, "attach evidence to world w (kind = 'ticket', uri = 'https://t/42')", &[]).unwrap();
    let out = run(&db, MAIN, "explain merge world w", &[]).unwrap();
    let cols: Vec<&str> = out[0].columns.as_ref().unwrap().iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(cols, ["section", "item", "detail"]);
    let l = lines(&out);
    assert!(has(&l, "counts", "t"), "{l:?}");
    assert!(has(&l, "counts", "2 updated"), "{l:?}");
    assert!(has(&l, "policy", "small"), "{l:?}");
    assert!(has(&l, "policy", "held"), "{l:?}");
    assert!(has(&l, "reason", "at most 1"), "{l:?}");
    assert!(has(&l, "reader", "nightly_export"), "{l:?}");
    assert!(has(&l, "effect", "billing"), "{l:?}");
    assert!(has(&l, "evidence", "ticket"), "{l:?}");
    assert!(has(&l, "certificate", ""), "{l:?}");
    // the certificate it gives is the one a proposal made now carries
    let cert = certificate(&db, "w");
    let proposed = bot.run(&db, "propose merge world w", &[]).unwrap();
    assert_eq!(col(&proposed, "certificate"), cert);
    // an agent may explain its own world's merge too
    let mine = bot.run(&db, "explain merge world w", &[]).unwrap();
    assert!(has(&lines(&mine), "certificate", &cert));
    // a conflict shows
    run(&db, MAIN, "create world a; create world b", &[]).unwrap();
    run(&db, "a", "update t set v = 'a' where id = 5", &[]).unwrap();
    run(&db, "b", "update t set v = 'b' where id = 5", &[]).unwrap();
    run(&db, MAIN, "merge world a", &[]).unwrap();
    let l = lines(&run(&db, MAIN, "explain merge world b", &[]).unwrap());
    assert!(has(&l, "conflict", "t/5"), "{l:?}");
}

#[test]
fn provenance_for_merge_returns_chain() {
    let dir = std::env::temp_dir().join(format!("chronos-provenance-chain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    setup(&db);
    run(&db, MAIN, "create merge policy small with (max_rows = 1)", &[]).unwrap();
    let mut bot = agent(&db, "bot", &WRITER, Some("small"));
    bot.run(
        &db,
        "create world w with (run_id = 'r7', model = 'claude-x'); use world w;
         update t set amount = 0 where id in (1, 2); use world main",
        &[],
    )
    .unwrap();
    bot.run(&db, "attach evidence to world w (kind = 'ticket', uri = 'https://t/7', sha256 = 'ab12')", &[]).unwrap();
    let out = bot.run(&db, "propose merge world w", &[]).unwrap();
    let (id, cert) = (col(&out, "id"), col(&out, "certificate"));
    run(&db, MAIN, &format!("approve proposal '{id}' reason 'refunds checked'"), &[]).unwrap();
    run(&db, MAIN, &format!("merge proposal '{id}'"), &[]).unwrap();

    let out = run(&db, MAIN, &format!("show decision provenance for merge '{id}'"), &[]).unwrap();
    let cols: Vec<&str> = out[0].columns.as_ref().unwrap().iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(cols, ["step", "at", "by", "detail"]);
    let l = lines(&out);
    assert!(has(&l, "proposal", "w"), "{l:?}");
    assert!(has(&l, "proposal", "bot"), "{l:?}");
    assert!(has(&l, "certificate", &cert), "{l:?}");
    assert!(has(&l, "verdict", "held"), "{l:?}");
    assert!(has(&l, "evidence", "ticket"), "{l:?}");
    assert!(has(&l, "policy", "small"), "{l:?}");
    assert!(has(&l, "run", "r7"), "{l:?}");
    assert!(has(&l, "approval", "refunds checked"), "{l:?}");
    assert!(has(&l, "approval", "person"), "{l:?}");
    assert!(has(&l, "merge", "bot"), "{l:?}");
    assert!(has(&l, "audit", "fork from main"), "{l:?}");
    // history past retention may be gone: it says so
    assert!(has(&l, "retention", "may be missing"), "{l:?}");
    // the same, found by the world's name once it's merged
    let by_world = lines(&run(&db, MAIN, "show decision provenance for world w", &[]).unwrap());
    assert!(has(&by_world, "certificate", &cert), "{by_world:?}");
    // an unknown one
    assert_eq!(run(&db, MAIN, "show decision provenance for merge 'nope'", &[]).unwrap_err().code, "42704");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tombstone_written_on_discard_and_refusal() {
    let db = Db::new();
    setup(&db);
    let mut bot = agent(&db, "bot", &WRITER, None);
    let mut other = agent(&db, "other", &WRITER, None);
    bot.run(
        &db,
        "create world gone with (run_id = 'r1'); use world gone; delete from t where id = 1;
         insert into t values (9, 'nine', 90); use world main; drop world gone",
        &[],
    )
    .unwrap();
    other.run(&db, "create world others; drop world others", &[]).unwrap();
    // a world no agent owns leaves none
    run(&db, MAIN, "create world plain; drop world plain", &[]).unwrap();

    let out = run(&db, MAIN, "show tombstones", &[]).unwrap();
    let cols: Vec<String> = out[0].columns.as_ref().unwrap().iter().map(|(c, _)| c.clone()).collect();
    for c in ["id", "world", "owner", "kind", "run_id", "parent", "born", "depth", "digest", "counts", "reasons", "at"]
    {
        assert!(cols.iter().any(|x| x == c), "{c} in {cols:?}");
    }
    let at = |c: &str| cols.iter().position(|x| x == c).unwrap();
    let rows = &out[0].rows;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let gone = rows.iter().find(|r| r[at("world")].to_text() == "gone").unwrap();
    assert_eq!(gone[at("owner")].to_text(), "bot");
    assert_eq!(gone[at("kind")].to_text(), "discarded");
    assert_eq!(gone[at("run_id")].to_text(), "r1");
    assert_eq!(gone[at("parent")].to_text(), "main");
    assert_eq!(gone[at("depth")].to_text(), "1");
    assert_eq!(gone[at("digest")].to_text().len(), 64);
    assert!(gone[at("counts")].to_text().contains("2 rows"), "{:?}", gone[at("counts")]);

    // a refused merge leaves one too, with why
    run(&db, MAIN, "create merge check no_nines on tables (t) as select id from t where id = 9", &[]).unwrap();
    bot.run(&db, "create world refused; use world refused; insert into t values (9, 'nine', 90); use world main", &[])
        .unwrap();
    assert_eq!(bot.run(&db, "merge world refused", &[]).unwrap_err().code, "23514");
    // and a rejected proposal
    bot.run(&db, "create world turned; use world turned; insert into t values (8, 'eight', 80); use world main", &[])
        .unwrap();
    let id = col(&bot.run(&db, "propose merge world turned", &[]).unwrap(), "id");
    run(&db, MAIN, &format!("reject proposal '{id}' reason 'not now'"), &[]).unwrap();

    let out = run(&db, MAIN, "show tombstones for agent bot", &[]).unwrap();
    let rows = &out[0].rows;
    assert!(rows.iter().all(|r| r[at("owner")].to_text() == "bot"), "{rows:?}");
    let kinds: Vec<(String, String)> =
        rows.iter().map(|r| (r[at("world")].to_text(), r[at("kind")].to_text())).collect();
    assert!(kinds.contains(&("refused".into(), "refused".into())), "{kinds:?}");
    assert!(kinds.contains(&("turned".into(), "rejected".into())), "{kinds:?}");
    let refused = rows.iter().find(|r| r[at("world")].to_text() == "refused").unwrap();
    assert!(refused[at("reasons")].to_text().contains("no_nines"), "{:?}", refused[at("reasons")]);
    let turned = rows.iter().find(|r| r[at("world")].to_text() == "turned").unwrap();
    assert!(turned[at("reasons")].to_text().contains("not now"), "{:?}", turned[at("reasons")]);
    // an agent sees its own; another's are for those who list everything
    let mine = bot.run(&db, "show tombstones for agent bot", &[]).unwrap();
    assert_eq!(mine[0].rows.len(), rows.len());
    assert_eq!(bot.run(&db, "show tombstones for agent other", &[]).unwrap_err().code, "42501");
}

#[test]
fn certificate_covers_hidden_rows() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(&db, "w", "update t set v = 'x' where id = 1", &[]).unwrap();
    let before = certificate(&db, "w");
    let diff = |db: &Db| format!("{:?}", run(db, MAIN, "diff world w", &[]).unwrap()[0].rows);
    let shown = diff(&db);
    // the same again: the same certificate
    assert_eq!(certificate(&db, "w"), before);
    // an effect queued (a row the diff doesn't show) changes it
    run(&db, "w", "notify on merge billing, 'refund 1'", &[]).unwrap();
    assert_eq!(diff(&db), shown, "the diff shows the same rows");
    let after = certificate(&db, "w");
    assert_ne!(after, before, "the certificate covers every row the merge would write");
    // and so does evidence attached
    run(&db, MAIN, "attach evidence to world w (kind = 'ticket')", &[]).unwrap();
    assert_ne!(certificate(&db, "w"), after);
}
