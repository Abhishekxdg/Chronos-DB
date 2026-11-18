//! Certified merges: PROPOSE MERGE freezes a world and binds what its merge would do to a certificate hash, APPROVE
//! PROPOSAL binds an approval to that hash, and MERGE PROPOSAL derives the certificate again and merges as the
//! proposing agent, its merge policy judged again with only what the approval covers lifted.

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

/// Column `col` of the first row of the last output.
fn col(out: &[Output], col: &str) -> String {
    let o = out.last().unwrap();
    let cols = o.columns.as_ref().unwrap();
    let at = cols.iter().position(|(c, _)| c == col).unwrap_or_else(|| panic!("no column {col}: {cols:?}"));
    o.rows[0][at].to_text()
}

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create table t (id int primary key, v text);
         insert into t select g, 'v' || g from generate_series(1, 5) g",
        &[],
    )
    .unwrap();
}

fn status(db: &Db, id: &str) -> String {
    let out = run(db, MAIN, "show proposals", &[]).unwrap();
    let o = out.last().unwrap();
    let cols = o.columns.as_ref().unwrap();
    let (i, s) =
        (cols.iter().position(|(c, _)| c == "id").unwrap(), cols.iter().position(|(c, _)| c == "status").unwrap());
    o.rows.iter().find(|r| r[i].to_text() == id).map(|r| r[s].to_text()).unwrap_or_default()
}

fn count(db: &Db, sql: &str) -> String {
    run(db, MAIN, sql, &[]).unwrap().pop().unwrap().rows[0][0].to_text()
}

#[test]
fn propose_freezes_world() {
    let db = Db::new();
    setup(&db);
    let mut bot = agent(&db, "bot", &WRITER, None);
    bot.run(&db, "create world w; use world w; insert into t values (10, 'x'); use world main", &[]).unwrap();
    let out = bot.run(&db, "propose merge world w", &[]).unwrap();
    let id = col(&out, "id");
    assert_eq!(col(&out, "status"), "open");
    assert_eq!(col(&out, "verdict"), "passes");
    assert_eq!(col(&out, "certificate").len(), 64, "a blake3 hash");

    // the agent's writes to it are refused while the proposal is open
    let e = bot.run(&db, "use world w; insert into t values (11, 'y')", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    assert!(e.message.contains("proposal"), "{}", e.message);
    bot.run(&db, "use world main", &[]).unwrap();
    // and so are a fork of it and a merge past the proposal
    let e = bot.run(&db, "create world w2 from w", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    let e = bot.run(&db, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    // one proposal at a time
    let e = bot.run(&db, "propose merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    assert_eq!(status(&db, &id), "open");

    // withdrawn: the world is the agent's to change again
    let out = bot.run(&db, &format!("withdraw proposal '{id}'"), &[]).unwrap();
    assert_eq!(out[0].tag, "WITHDRAW PROPOSAL");
    assert_eq!(status(&db, &id), "withdrawn");
    bot.run(&db, "use world w; insert into t values (11, 'y'); use world main", &[]).unwrap();
    // a withdrawn proposal stays withdrawn
    assert_eq!(bot.run(&db, &format!("withdraw proposal '{id}'"), &[]).unwrap_err().code, "55000");
    // another agent can't withdraw what it didn't propose
    let id2 = col(&bot.run(&db, "propose merge world w", &[]).unwrap(), "id");
    assert_ne!(id2, id, "a new proposal, a new id");
    let mut other = agent(&db, "other", &WRITER, None);
    assert_eq!(other.run(&db, &format!("withdraw proposal '{id2}'"), &[]).unwrap_err().code, "42501");
    // nor propose a world it can't merge
    run(&db, MAIN, "create world mine", &[]).unwrap();
    assert_eq!(other.run(&db, "propose merge world mine", &[]).unwrap_err().code, "42501");
    // an unknown proposal
    assert_eq!(run(&db, MAIN, "withdraw proposal 'nope'", &[]).unwrap_err().code, "42704");
}

#[test]
fn approve_binds_to_hash_and_write_after_approval_refuses_merge() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create merge policy small with (max_rows = 1)", &[]).unwrap();
    let mut bot = agent(&db, "bot", &WRITER, Some("small"));
    bot.run(&db, "create world w; use world w; insert into t values (10, 'x'), (11, 'y'); use world main", &[])
        .unwrap();
    let out = bot.run(&db, "propose merge world w", &[]).unwrap();
    let (id, cert) = (col(&out, "id"), col(&out, "certificate"));
    assert_eq!(col(&out, "verdict"), "held");
    assert!(col(&out, "reasons").contains("at most 1"), "{}", col(&out, "reasons"));

    // a person approves it: bound to its certificate
    let out = run(&db, MAIN, &format!("approve proposal '{id}' reason 'two rows is fine'"), &[]).unwrap();
    assert_eq!(out[0].tag, "APPROVE PROPOSAL");
    assert_eq!(status(&db, &id), "approved");
    let shown = run(&db, MAIN, "show proposals", &[]).unwrap();
    assert!(format!("{:?}", shown[0].rows).contains(&cert), "{:?}", shown[0].rows);

    // then the world changes (a person may still write to it): the approval no longer covers what would merge
    run(&db, "w", "update t set v = 'changed' where id = 10", &[]).unwrap();
    let e = run(&db, MAIN, &format!("merge proposal '{id}'"), &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed since"), "{}", e.message);
    assert!(db.world("w").is_ok(), "nothing merged");
    assert_eq!(count(&db, "select count(*) from t where id >= 10"), "0");

    // a world that changed between proposal and approval: the approval is refused
    bot.run(&db, "create world w3; use world w3; insert into t values (30, 'x'), (31, 'y'); use world main", &[])
        .unwrap();
    let id3 = col(&bot.run(&db, "propose merge world w3", &[]).unwrap(), "id");
    run(&db, "w3", "delete from t where id = 31", &[]).unwrap();
    let e = run(&db, MAIN, &format!("approve proposal '{id3}'"), &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);

    // one that didn't change merges, past the hold the approval covers
    bot.run(&db, "create world w2; use world w2; insert into t values (20, 'x'), (21, 'y'); use world main", &[])
        .unwrap();
    let id2 = col(&bot.run(&db, "propose merge world w2", &[]).unwrap(), "id");
    // not before it's approved
    let e = run(&db, MAIN, &format!("merge proposal '{id2}'"), &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    run(&db, MAIN, &format!("approve proposal '{id2}'"), &[]).unwrap();
    let out = run(&db, MAIN, &format!("merge proposal '{id2}'"), &[]).unwrap();
    assert_eq!(out[0].tag, "MERGE PROPOSAL 2");
    assert!(db.world("w2").is_err(), "merged");
    assert_eq!(count(&db, "select count(*) from t where id >= 20 and id < 30"), "2");
    assert_eq!(status(&db, &id2), "merged");
    // merged once
    assert_eq!(run(&db, MAIN, &format!("merge proposal '{id2}'"), &[]).unwrap_err().code, "55000");
}

#[test]
fn merge_proposal_rejudges_policy_as_proposer() {
    let dir = std::env::temp_dir().join(format!("chronos-proposals-rejudge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    setup(&db);
    run(&db, MAIN, "create merge policy careful with (max_deletes = 0, reviewer = 'critic')", &[]).unwrap();
    let mut bot = agent(&db, "bot", &WRITER, Some("careful"));
    let mut critic = agent(&db, "critic", &[Can::Read], None);
    bot.run(&db, "create world w; use world w; delete from t where id = 1; use world main", &[]).unwrap();
    let out = bot.run(&db, "propose merge world w", &[]).unwrap();
    let id = col(&out, "id");
    assert_eq!(col(&out, "verdict"), "held");

    // the policy's reviewer approves; but deletes are for a person (person_only by default), and the merge is the
    // agent's, judged again as its own: still held, where `REVIEW WORLD ... APPROVE` would merge as a person
    critic.run(&db, &format!("approve proposal '{id}' reason 'a duplicate'"), &[]).unwrap();
    let e = run(&db, MAIN, &format!("merge proposal '{id}'"), &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("deletes"), "{}", e.message);
    assert!(db.world("w").is_ok(), "not merged");
    assert_eq!(count(&db, "select count(*) from t"), "5");

    // a person's approval covers it: merged, by the agent
    run(&db, MAIN, &format!("approve proposal '{id}' reason 'checked the deal'"), &[]).unwrap();
    run(&db, MAIN, &format!("merge proposal '{id}'"), &[]).unwrap();
    assert!(db.world("w").is_err(), "merged");
    assert_eq!(count(&db, "select count(*) from t"), "4");
    let audit = run(&db, MAIN, "show audit for agent bot", &[]).unwrap().pop().unwrap();
    let actions: Vec<String> = audit.rows.iter().map(|r| format!("{} {}", r[2].to_text(), r[3].to_text())).collect();
    assert!(actions.contains(&"w merge into main".to_string()), "{actions:?}");

    // the policy changing after an approval changes the certificate: refused
    bot.run(&db, "create world v; use world v; delete from t where id = 2; use world main", &[]).unwrap();
    let id = col(&bot.run(&db, "propose merge world v", &[]).unwrap(), "id");
    run(&db, MAIN, &format!("approve proposal '{id}'"), &[]).unwrap();
    run(&db, MAIN, "alter merge policy careful set (max_rows = 100)", &[]).unwrap();
    let e = run(&db, MAIN, &format!("merge proposal '{id}'"), &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_cannot_approve_own_proposal() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create merge policy small with (max_rows = 1, reviewer = 'critic')", &[]).unwrap();
    let mut bot = agent(&db, "bot", &[WRITER.as_slice(), &[Can::Admin]].concat(), Some("small"));
    let mut critic = agent(&db, "critic", &[Can::Read], None);
    let mut other = agent(&db, "other", &[Can::Read], None);
    bot.run(&db, "create world w; use world w; insert into t values (10, 'x'), (11, 'y'); use world main", &[])
        .unwrap();
    let id = col(&bot.run(&db, "propose merge world w", &[]).unwrap(), "id");

    // not its own, even as an admin
    let e = bot.run(&db, &format!("approve proposal '{id}'"), &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("own"), "{}", e.message);
    // nor an agent that isn't the policy's reviewer
    let e = other.run(&db, &format!("approve proposal '{id}'"), &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert_eq!(status(&db, &id), "open");
    // the reviewer may, and its approval covers max_rows (not person_only): the merge goes through as the agent's
    critic.run(&db, &format!("approve proposal '{id}' reason 'fine'"), &[]).unwrap();
    assert_eq!(status(&db, &id), "approved");
    bot.run(&db, &format!("merge proposal '{id}'"), &[]).unwrap();
    assert!(db.world("w").is_err(), "merged");

    // rejected: the world is the agent's to change again, and the rejection is remembered
    bot.run(&db, "create world r; use world r; insert into t values (20, 'x'), (21, 'y'); use world main", &[])
        .unwrap();
    let id = col(&bot.run(&db, "propose merge world r", &[]).unwrap(), "id");
    assert_eq!(bot.run(&db, &format!("reject proposal '{id}'"), &[]).unwrap_err().code, "42501");
    let out = critic.run(&db, &format!("reject proposal '{id}' reason 'needs a ticket'"), &[]).unwrap();
    assert_eq!(out[0].tag, "REJECT PROPOSAL");
    assert_eq!(status(&db, &id), "rejected");
    bot.run(&db, "use world r; insert into t values (22, 'z'); use world main", &[]).unwrap();
    assert_eq!(run(&db, MAIN, &format!("approve proposal '{id}'"), &[]).unwrap_err().code, "55000");
}

/// A proposal merged into main is merged in the merge's own log record (`Op::Applied`, beside its ledger row): a crash
/// right after that record, before anything else is logged, opens with the world merged and the proposal `merged`,
/// never `approved` with its world gone.
#[test]
fn merged_proposal_row_is_in_the_merge_record() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("chronos-test-{}-proposal-applied", std::process::id()));
    let crash = dir.with_extension("crash");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&crash);
    let segments = |d: &std::path::Path| {
        let mut s: Vec<_> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("wal-"))
            .collect();
        s.sort();
        s
    };
    // (tag, start, end) of each record of a segment
    let frames = |b: &[u8]| {
        let (mut off, mut out) = (0, vec![]);
        while off + 8 <= b.len() {
            let len = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
            if len == 0 {
                break;
            }
            out.push((b[off + 8], off, off + 8 + len));
            off += 8 + len;
        }
        out
    };
    let db = Db::open(&dir).unwrap();
    setup(&db);
    let mut bot = agent(&db, "bot", &WRITER, None);
    bot.run(&db, "create world w; use world w; insert into t values (10, 'x'); use world main", &[]).unwrap();
    let id = col(&bot.run(&db, "propose merge world w", &[]).unwrap(), "id");
    run(&db, MAIN, &format!("approve proposal '{id}'"), &[]).unwrap();
    assert_eq!(run(&db, MAIN, &format!("merge proposal '{id}'"), &[]).unwrap()[0].tag, "MERGE PROPOSAL 1");
    assert_eq!(status(&db, &id), "merged");
    // the crash image: the folder as it is, its last segment cut right after the merge's record
    fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap() {
            let p = e.unwrap().path();
            let dest = to.join(p.file_name().unwrap());
            if p.is_dir() {
                copy_dir(&p, &dest)
            } else {
                fs::copy(&p, &dest).unwrap();
            }
        }
    }
    copy_dir(&dir, &crash);
    let seg = segments(&crash).pop().unwrap();
    let b = fs::read(&seg).unwrap();
    let all = frames(&b);
    let &(_, start, end) = all.iter().rev().find(|(t, ..)| *t == 20).expect("the merge's Op::Applied");
    let key = format!("_sonos_proposals/{id}");
    let has = |from: usize, to: usize| b[from..to].windows(key.len()).any(|w| w == key.as_bytes());
    assert!(has(start, end), "the proposal's row is in the merge's record");
    assert!(!all.iter().filter(|(_, s, _)| *s >= end).any(|&(_, s, e)| has(s, e)), "and not written again after it");
    fs::write(&seg, &b[..end]).unwrap();
    drop(db);
    let db = Db::open(&crash).unwrap();
    assert!(db.world("w").is_err(), "merged");
    assert_eq!(count(&db, "select count(*) from t where id = 10"), "1");
    assert_eq!(status(&db, &id), "merged");
    drop(db);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&crash);
}

/// MERGE PROPOSAL merges as the proposing agent with its rights as they are then, not as they were when it proposed:
/// the world transferred away from it after the approval (its lease lost: 55P03), or its rights taken away (42501),
/// and nothing merges. Its own proposal's freeze is all the merge goes past.
#[test]
fn merge_proposal_rechecks_current_rights() {
    let db = Db::new();
    setup(&db);
    let mut bot = agent(&db, "bot", &WRITER, None);
    agent(&db, "other", &WRITER, None);
    bot.run(&db, "create world w; use world w; insert into t values (10, 'x'); use world main", &[]).unwrap();
    let id = col(&bot.run(&db, "propose merge world w", &[]).unwrap(), "id");
    run(&db, MAIN, &format!("approve proposal '{id}'"), &[]).unwrap();
    run(&db, MAIN, "transfer world w to other", &[]).unwrap();
    let e = bot.run(&db, &format!("merge proposal '{id}'"), &[]).unwrap_err();
    assert_eq!(e.code, "55P03", "{}", e.message);
    let e = run(&db, MAIN, &format!("merge proposal '{id}'"), &[]).unwrap_err();
    assert_eq!(e.code, "55P03", "a person merging it merges as the agent all the same: {}", e.message);
    assert_eq!(count(&db, "select count(*) from t where id = 10"), "0", "nothing merged");
    assert_eq!(status(&db, &id), "approved");

    // rights taken away after the approval
    bot.run(&db, "create world w2; use world w2; insert into t values (20, 'x'); use world main", &[]).unwrap();
    let id2 = col(&bot.run(&db, "propose merge world w2", &[]).unwrap(), "id");
    run(&db, MAIN, &format!("approve proposal '{id2}'"), &[]).unwrap();
    run(&db, MAIN, "alter agent bot set (can = 'read')", &[]).unwrap();
    let e = run(&db, MAIN, &format!("merge proposal '{id2}'"), &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert_eq!(count(&db, "select count(*) from t where id = 20"), "0", "nothing merged");
    // given back: it merges, past its own proposal's freeze
    run(&db, MAIN, "alter agent bot set (can = 'read,fork,write_own,write_main,merge_own')", &[]).unwrap();
    let out = run(&db, MAIN, &format!("merge proposal '{id2}'"), &[]).unwrap();
    assert_eq!(out[0].tag, "MERGE PROPOSAL 1");
    assert_eq!(count(&db, "select count(*) from t where id = 20"), "1");
    assert_eq!(status(&db, &id2), "merged");
}

/// Under a merge policy with a canary, an approved proposal merges whole: a canary's part first would no longer match
/// the certificate the person approved, and the proposal could never merge.
#[test]
fn an_approved_proposal_merges_whole_under_a_canary() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create merge policy c with (canary = 1, canary_wait = '1 hour', max_rows = 1)", &[]).unwrap();
    let mut bot = agent(&db, "bot", &WRITER, Some("c"));
    bot.run(
        &db,
        "create world w; use world w; insert into t values (40, 'a'), (41, 'b'), (42, 'c'); use world main",
        &[],
    )
    .unwrap();
    let id = col(&bot.run(&db, "propose merge world w", &[]).unwrap(), "id");
    run(&db, MAIN, &format!("approve proposal '{id}' reason 'fine'"), &[]).unwrap();
    bot.run(&db, &format!("merge proposal '{id}'"), &[]).unwrap();
    assert_eq!(count(&db, "select count(*) from t where id >= 40"), "3");
    assert_eq!(status(&db, &id), "merged");
}
