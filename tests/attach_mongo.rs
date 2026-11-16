//! `chronos attach` beside a MongoDB replica set, against a real one. CHRONOS_TEST_MONGO is a URL to a server where
//! tests may create databases (`mongodb://127.0.0.1:27017/?directConnection=true`; skipped without it). Attach copies
//! the collections; a merge into main is written to MongoDB first in one transaction, a document changed there
//! meanwhile comes back as a conflict with nothing written; a refresh brings main up to MongoDB; a merge is undone by
//! its id; a dry run compares with MongoDB and writes nothing; a world's reads are checked against MongoDB.
#![cfg(feature = "mongo")]

use chronos::sql::run;
use chronos::{Db, MAIN};
use mongodb::bson::{Bson, Document, doc, oid::ObjectId};
use mongodb::sync::Client;
use std::process::Command;

fn cell(db: &Db, world: &str, sql: &str) -> String {
    run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap().rows[0][0].to_text()
}

/// CHRONOS_SOURCE_URL is the process's: one test at a time.
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A new database `chronos_<tag>_<pid>` on the test server, CHRONOS_SOURCE_URL set to it.
fn throwaway(tag: &str) -> Option<(Client, mongodb::sync::Database)> {
    let admin = std::env::var("CHRONOS_TEST_MONGO").ok()?;
    let name = format!("chronos_{tag}_{}", std::process::id());
    let (base, query) = admin.split_once('?').map_or((admin.as_str(), ""), |(b, q)| (b, q));
    let base = base.trim_end_matches('/');
    // (the URL's own database, if it names one, gives way to the throwaway)
    let base = match base.split_once("://").map(|(_, rest)| rest.contains('/')) {
        Some(true) => base.rsplit_once('/').unwrap().0,
        _ => base,
    };
    let url = format!("{base}/{name}{}{query}", if query.is_empty() { "" } else { "?" });
    unsafe { std::env::set_var("CHRONOS_SOURCE_URL", &url) };
    let client = Client::with_uri_str(&url).unwrap();
    let db = client.database(&name);
    db.drop().run().unwrap();
    Some((client, db))
}

fn attach(dir: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_chronos")).arg("attach").arg(dir).args(args).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

#[test]
fn merges_are_written_to_mongodb_first() {
    let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
    let Some((_client, mdb)) = throwaway("attach") else {
        eprintln!("skipped: set CHRONOS_TEST_MONGO to a MongoDB URL");
        return;
    };
    let leads = mdb.collection::<Document>("leads");
    let ada = ObjectId::new();
    leads
        .insert_many([
            doc! { "_id": ada, "org_id": 42, "name": "Acme", "owner": "bo", "score": 1.5, "visits": 3_i64,
            "seen": mongodb::bson::DateTime::from_millis(1_790_000_000_000), "tags": ["a", "b"],
            "address": { "city": "Pune", "zip": "411001" } },
            doc! { "_id": "lead-2", "org_id": 42, "name": "Initech", "owner": "bo", "score": 2.0 },
            doc! { "_id": 3, "org_id": 7, "name": "Globex", "owner": "eve" },
        ])
        .run()
        .unwrap();
    mdb.create_collection("events").capped(true).size(4096).run().unwrap();
    let dir = std::env::temp_dir().join(format!("chronos-attach-mongo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // a capped collection is refused, nothing left behind
    let (ok, out) = attach(&dir, &["--tables", "leads,events"]);
    assert!(!ok && out.contains("capped"), "{out}");
    assert!(!dir.exists());
    let (ok, out) = attach(&dir, &["--tables", "leads", "--fields", "org_id"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let att = db.attached().unwrap();
    assert_eq!(att.kind().as_str(), "mongodb");
    assert!(att.capabilities().follow && att.capabilities().dry_run && att.capabilities().check_reads);
    assert!(!att.capabilities().act_as && !att.capabilities().lazy_tenant);
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "3");
    // a document is a row: _id, the field kept as a column, and the rest as jsonb
    let hex = ada.to_hex();
    assert_eq!(cell(&db, MAIN, &format!("select doc->>'name' from leads where _id = '{hex}'")), "Acme");
    assert_eq!(
        cell(&db, MAIN, "select string_agg(_id, ',' order by _id) from leads where org_id = 42"),
        format!("\"lead-2\",{hex}")
    );
    assert_eq!(cell(&db, MAIN, &format!("select doc->'address'->>'city' from leads where _id = '{hex}'")), "Pune");

    // a merge: written to MongoDB first, each kind of change, only the fields that changed
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(
        &db,
        "w",
        &format!(
            "update leads set doc = jsonb_set(jsonb_set(doc, '{{owner}}', '\"ada\"'), '{{address,city}}', '\"Mumbai\"')
               where _id = '{hex}';
             update leads set doc = jsonb_set(doc, '{{score}}', '9') where _id = '\"lead-2\"';
             update leads set doc = jsonb_set(doc, '{{visits}}', '4') where _id = '{hex}';
             insert into leads (org_id, doc) values (42, '{{\"name\": \"NewCo\", \"owner\": \"ada\", \"n\": 5, \"at\": {{\"$date\": \"2026-10-01T10:00:00Z\"}}}}');
             delete from leads where _id = '3'"
        ),
        &[],
    )
    .unwrap();
    let _ = chronos::attach::take_merge_id();
    run(&db, MAIN, "merge world w", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    let first = chronos::attach::take_merge_id().expect("the merge's id");
    let got = leads.find_one(doc! { "_id": ada }).run().unwrap().unwrap();
    assert_eq!(got.get_str("owner").ok(), Some("ada"));
    assert_eq!(got.get_document("address").unwrap().get_str("city").ok(), Some("Mumbai"));
    assert_eq!(got.get_document("address").unwrap().get_str("zip").ok(), Some("411001"), "a field it left");
    // types kept: the long stays a long, the date a date, the tenant an int, the fields their order
    assert_eq!(got.get("visits"), Some(&Bson::Int64(4)));
    assert!(matches!(got.get("seen"), Some(Bson::DateTime(_))));
    assert_eq!(got.get("org_id"), Some(&Bson::Int32(42)));
    assert_eq!(got.keys().take(4).collect::<Vec<_>>(), ["_id", "org_id", "name", "owner"]);
    // a double stays a double though the world wrote a whole number
    let two = leads.find_one(doc! { "_id": "lead-2" }).run().unwrap().unwrap();
    assert_eq!(two.get("score"), Some(&Bson::Double(9.0)));
    assert!(leads.find_one(doc! { "_id": 3 }).run().unwrap().is_none());
    let new = leads.find_one(doc! { "name": "NewCo" }).run().unwrap().unwrap();
    assert!(matches!(new.get("_id"), Some(Bson::ObjectId(_))), "{new:?}");
    assert_eq!((new.get("n"), new.get("org_id")), (Some(&Bson::Int32(5)), Some(&Bson::Int32(42))));
    assert!(matches!(new.get("at"), Some(Bson::DateTime(_))));
    assert_eq!(mdb.collection::<Document>("chronos_merges").count_documents(doc! {}).run().unwrap(), 2);
    // main holds MongoDB's documents: nothing to refresh
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "3");
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0);

    // a document changed in MongoDB since: a conflict, nothing written, main takes MongoDB's
    run(&db, MAIN, "create world c", &[]).unwrap();
    run(&db, "c", &format!("update leads set doc = jsonb_set(doc, '{{owner}}', '\"cy\"') where _id = '{hex}'"), &[])
        .unwrap();
    run(&db, "c", "update leads set doc = jsonb_set(doc, '{name}', '\"Initech 2\"') where _id = '\"lead-2\"'", &[])
        .unwrap();
    leads.update_one(doc! { "_id": ada }, doc! { "$set": { "owner": "theirs" } }).run().unwrap();
    let e = run(&db, MAIN, "merge world c", &[]).unwrap_err();
    assert!(e.code == "40001" && e.message.contains("changed in MongoDB"), "{}", e.message);
    assert_eq!(leads.find_one(doc! { "_id": "lead-2" }).run().unwrap().unwrap().get_str("name").ok(), Some("Initech"));
    assert_eq!(cell(&db, MAIN, &format!("select doc->>'owner' from leads where _id = '{hex}'")), "theirs");

    // a refresh: MongoDB's inserts, updates and deletes
    leads.insert_one(doc! { "_id": 20, "org_id": 42, "name": "twenty" }).run().unwrap();
    leads.update_one(doc! { "_id": "lead-2" }, doc! { "$set": { "owner": "zed" } }).run().unwrap();
    leads.delete_one(doc! { "name": "NewCo" }).run().unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert_eq!(r.tables.iter().find(|t| t.0 == "leads").map(|t| (t.1, t.2, t.3)), Some((1, 1, 1)), "{r:?}");
    assert_eq!(cell(&db, MAIN, "select doc->>'name' from leads where _id = '20'"), "twenty");

    // the first merge undone by its id: what it changed goes back, but for the documents changed since
    let plan = db
        .undo_merges(std::slice::from_ref(&first), chronos::attach::UndoOptions { dry_run: true, ..Default::default() })
        .unwrap();
    assert_eq!(plan.rows.len(), 4, "{plan:?}");
    let done = db
        .undo_merges(
            std::slice::from_ref(&first),
            chronos::attach::UndoOptions { skip_changed: true, ..Default::default() },
        )
        .unwrap();
    // (lead 3 comes back; Acme and lead-2 changed since; NewCo, which the merge added, is gone already: nothing to
    // undo there)
    assert_eq!((done.undone, done.skipped, done.already), (1, 2, 1), "{done:?}");
    assert_eq!(leads.find_one(doc! { "_id": 3 }).run().unwrap().unwrap().get_str("owner").ok(), Some("eve"));
    assert_eq!(leads.find_one(doc! { "_id": 3 }).run().unwrap().unwrap().get("org_id"), Some(&Bson::Int32(7)));

    // a dry run: each document the merge would write compared with MongoDB's in one snapshot, nothing written
    run(&db, MAIN, "create world d", &[]).unwrap();
    run(&db, "d", "update leads set doc = jsonb_set(doc, '{name}', '\"x\"') where _id = '20'", &[]).unwrap();
    run(&db, "d", "insert into leads (org_id, doc) values (42, '{\"name\": \"Dry\"}')", &[]).unwrap();
    let live = db.merge_preview("d", &Default::default()).unwrap().live.expect("checked against MongoDB");
    assert!(live.error.is_none(), "{:?}", live.error);
    assert!(live.rows.len() == 2 && live.rows.iter().all(|r| !r.changed), "{live:?}");
    assert_eq!(leads.find_one(doc! { "_id": 20 }).run().unwrap().unwrap().get_str("name").ok(), Some("twenty"));
    // a document MongoDB changed since: the dry run says so, and says MongoDB's
    leads.update_one(doc! { "_id": 20 }, doc! { "$set": { "name": "theirs" } }).run().unwrap();
    let live = db.merge_preview("d", &Default::default()).unwrap().live.unwrap();
    let changed: Vec<_> = live.rows.iter().filter(|r| r.changed).collect();
    assert_eq!((live.rows.len(), changed.len()), (2, 1), "{live:?}");
    assert!(changed[0].postgres.as_deref().is_some_and(|j| j.contains("theirs")), "{live:?}");
    assert_eq!(cell(&db, MAIN, "select doc->>'name' from leads where _id = '20'"), "twenty", "main isn't moved");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    mdb.drop().run().unwrap();
}

/// A world that checks its reads (`check_reads`): what it read is checked against MongoDB inside the write-back,
/// before anything is written, and by CHECK READS between steps. A document changed straight in MongoDB never
/// reaches main, so main's own check can't see it.
#[test]
fn reads_are_checked_against_mongodb() {
    let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
    let Some((_client, mdb)) = throwaway("reads") else {
        eprintln!("skipped: set CHRONOS_TEST_MONGO to a MongoDB URL");
        return;
    };
    let leads = mdb.collection::<Document>("leads");
    let suppressed = mdb.collection::<Document>("suppressed");
    let optout = mdb.collection::<Document>("optout");
    leads
        .insert_many([
            doc! { "_id": 1, "org_id": 42, "email": "ada@x.com", "status": "new" },
            doc! { "_id": 2, "org_id": 42, "email": "bo@x.com", "status": "new" },
            doc! { "_id": 3, "org_id": 7, "email": "cy@y.com", "status": "new" },
        ])
        .run()
        .unwrap();
    mdb.create_collection("suppressed").run().unwrap();
    mdb.create_collection("optout").run().unwrap();
    let dir = std::env::temp_dir().join(format!("chronos-attach-mongo-reads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, out) = attach(&dir, &["--tables", "leads,suppressed,optout", "--fields", "org_id,email"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    assert!(db.attached().unwrap().capabilities().check_reads);
    let merges = || mdb.collection::<Document>("chronos_merges").count_documents(doc! {}).run().unwrap() - 1;
    let status =
        |id: i32| leads.find_one(doc! { "_id": id }).run().unwrap().unwrap().get_str("status").unwrap().to_string();
    let ready = |w: &str, id: i32| {
        let sql = format!("update leads set doc = jsonb_set(doc, '{{status}}', '\"ready\"') where _id = '{id}'");
        run(&db, w, &sql, &[]).unwrap();
    };

    // a filtered scan that found nothing (by a field kept as a column: a condition on a field inside `doc` counts
    // as a read of the whole collection)
    run(&db, MAIN, "create world w with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "w", "select count(*) from suppressed where email = 'ada@x.com'"), "0");
    ready("w", 1);
    // a document its read couldn't have found: nothing it relied on moved
    suppressed.insert_one(doc! { "_id": 1, "org_id": 42, "email": "someone@else.com" }).run().unwrap();
    assert_eq!(run(&db, "w", "check reads", &[]).unwrap().pop().unwrap().rows.len(), 0);
    // the email itself, added straight into MongoDB: CHECK READS says so before any merge, and the merge is refused
    // with nothing written
    suppressed.insert_one(doc! { "_id": 2, "org_id": 42, "email": "ada@x.com" }).run().unwrap();
    let found = run(&db, MAIN, "check reads of world w", &[]).unwrap().pop().unwrap();
    assert_eq!(found.rows.len(), 1, "{:?}", found.rows);
    assert!(found.rows[0][1].to_text().contains("changed in MongoDB since it forked"), "{:?}", found.rows);
    let e = run(&db, MAIN, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in MongoDB since it forked"), "{}", e.message);
    assert_eq!(status(1), "new", "nothing written");
    assert_eq!(merges(), 0);
    assert!(db.world("w").is_ok(), "the world stays, to redo or drop");
    // (and the dry run says the same)
    let live = db.merge_preview("w", &Default::default()).unwrap().live.expect("checked against MongoDB");
    assert!(live.error.is_none() && live.stale_reads.len() == 1, "{live:?}");
    run(&db, MAIN, "drop world w", &[]).unwrap();

    // a key looked up that wasn't there, added in MongoDB since
    run(&db, MAIN, "create world k with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "k", "select count(*) from optout where _id = '\"bo@x.com\"'"), "0");
    ready("k", 2);
    optout.insert_one(doc! { "_id": "bo@x.com" }).run().unwrap();
    let e = run(&db, MAIN, "merge world k", &[]).unwrap_err();
    assert!(e.message.contains("optout"), "{}", e.message);
    assert_eq!(status(2), "new");
    run(&db, MAIN, "drop world k", &[]).unwrap();

    // a document read and then changed in MongoDB
    run(&db, MAIN, "create world r with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "r", "select doc->>'status' from leads where _id = '2'"), "new");
    ready("r", 1);
    leads.update_one(doc! { "_id": 2 }, doc! { "$set": { "status": "do_not_contact" } }).run().unwrap();
    let e = run(&db, MAIN, "merge world r", &[]).unwrap_err();
    assert!(e.message.contains("leads"), "{}", e.message);
    assert_eq!(merges(), 0);
    run(&db, MAIN, "drop world r", &[]).unwrap();

    // what it read is still as MongoDB has it: the merge goes through, reads checked
    chronos::attach::refresh(&db).unwrap();
    run(&db, MAIN, "create world ok with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "ok", "select count(*) from leads where doc->>'status' = 'new'"), "2");
    ready("ok", 1);
    run(&db, MAIN, "merge world ok", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(status(1), "ready");
    assert_eq!(merges(), 1);

    // a world that read for one tenant is held by that tenant's documents only: the app writing another tenant's
    // documents of a collection it read whole doesn't hold it, one of its own added in MongoDB does
    chronos::attach::refresh(&db).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent run42 with (can = '{can}', tenant = 'org_id = 42')"), &[]).unwrap();
    let work = |w: &str| {
        let mut s = chronos::sql::Session::new(MAIN);
        s.agent = Some("run42".into());
        let sql = format!(
            "create world {w} with (check_reads = true); use world {w}; select count(*) from leads;
             update leads set email = '{w}@x.com' where _id = '1'; use world main"
        );
        let out = s.run(&db, &sql, &[]).unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(out[2].rows[0][0].to_text(), "2", "its tenant's leads");
    };
    let email = || leads.find_one(doc! { "_id": 1 }).run().unwrap().unwrap().get_str("email").unwrap().to_string();
    work("others");
    leads.update_one(doc! { "_id": 3 }, doc! { "$set": { "status": "app" } }).run().unwrap();
    leads.insert_one(doc! { "_id": 30, "org_id": 7, "email": "new@y.com", "status": "new" }).run().unwrap();
    run(&db, MAIN, "merge world others", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(email(), "others@x.com");
    work("mine");
    leads.insert_one(doc! { "_id": 31, "org_id": 42, "email": "new@x.com", "status": "new" }).run().unwrap();
    let e = run(&db, MAIN, "merge world mine", &[]).unwrap_err();
    assert!(e.message.contains("changed in MongoDB since it forked"), "{}", e.message);
    assert!(!e.message.contains("more broadly"), "{}", e.message);
    assert_eq!(email(), "others@x.com", "nothing written");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    mdb.drop().run().unwrap();
}

/// `serve --follow` beside MongoDB: a change stream says which documents changed, and main takes those as MongoDB
/// has them, without reading the rest. The first follow takes its place and refreshes; a merge's own write coming
/// back takes nothing; a collection dropped is compared whole; a place forgotten starts again.
#[test]
fn main_follows_mongodb_through_a_change_stream() {
    let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
    let Some((_client, mdb)) = throwaway("follow") else {
        eprintln!("skipped: set CHRONOS_TEST_MONGO to a MongoDB URL");
        return;
    };
    let leads = mdb.collection::<Document>("leads");
    let notes = mdb.collection::<Document>("notes");
    let other = mdb.collection::<Document>("other");
    leads
        .insert_many([
            doc! { "_id": 1, "org_id": 42, "name": "Acme", "owner": "bo" },
            doc! { "_id": "lead-2", "org_id": 42, "name": "Initech", "owner": "bo" },
        ])
        .run()
        .unwrap();
    notes.insert_one(doc! { "_id": 1, "org_id": 42, "body": "call" }).run().unwrap();
    other.insert_one(doc! { "_id": 1, "x": 1 }).run().unwrap();
    let dir = std::env::temp_dir().join(format!("chronos-attach-mongo-follow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, said) = attach(&dir, &["--tables", "leads,notes", "--fields", "org_id"]);
    assert!(ok, "{said}");
    let db = Db::open(&dir).unwrap();
    let of = |r: &chronos::attach::Refreshed, t: &str| r.tables.iter().find(|x| x.0 == t).map(|x| (x.1, x.2, x.3));
    let place = dir.join("mongo-follow.json");

    // a change before the first follow: that follow takes its place, and refreshes
    leads.update_one(doc! { "_id": 1 }, doc! { "$set": { "owner": "early" } }).run().unwrap();
    assert!(!place.exists());
    let first = chronos::attach::follow(&db).unwrap();
    assert!(first.slot.as_deref().is_some_and(|s| s.contains("change stream")), "{first:?}");
    assert_eq!(of(&first, "leads"), Some((0, 1, 0)), "{first:?}");
    assert!(place.exists());
    assert_eq!(cell(&db, MAIN, "select doc->>'owner' from leads where _id = '1'"), "early");
    // nothing changed: nothing read, nothing taken
    let none = chronos::attach::follow(&db).unwrap();
    assert_eq!((none.changed(), none.tables.len(), none.slot.clone()), (0, 0, None), "{none:?}");

    // inserts, updates and deletes, in two collections; a collection that isn't attached is nobody's
    leads.insert_one(doc! { "_id": 20, "org_id": 42, "name": "twenty" }).run().unwrap();
    leads.update_one(doc! { "_id": "lead-2" }, doc! { "$set": { "owner": "zed" } }).run().unwrap();
    leads.update_one(doc! { "_id": "lead-2" }, doc! { "$set": { "owner": "zed2" } }).run().unwrap();
    leads.delete_one(doc! { "_id": 1 }).run().unwrap();
    notes.insert_one(doc! { "_id": 2, "org_id": 42, "body": "mail" }).run().unwrap();
    // (added and removed between two follows: there neither before nor after)
    notes.insert_one(doc! { "_id": 3, "org_id": 42, "body": "gone" }).run().unwrap();
    notes.delete_one(doc! { "_id": 3 }).run().unwrap();
    other.insert_one(doc! { "_id": 2, "x": 2 }).run().unwrap();
    let r = chronos::attach::follow(&db).unwrap();
    assert_eq!((of(&r, "leads"), of(&r, "notes")), (Some((1, 1, 1)), Some((1, 0, 0))), "{r:?}");
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "2");
    assert_eq!(cell(&db, MAIN, "select doc->>'owner' from leads where _id = '\"lead-2\"'"), "zed2");
    assert_eq!(cell(&db, MAIN, "select doc->>'name' from leads where _id = '20'"), "twenty");
    assert_eq!(cell(&db, MAIN, "select count(*) from notes"), "2");
    assert_eq!(chronos::attach::follow(&db).unwrap().changed(), 0);

    // a merge's own write comes back in the stream, and takes nothing: main has it already
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(&db, "w", "update leads set doc = jsonb_set(doc, '{owner}', '\"ada\"') where _id = '20'", &[]).unwrap();
    run(&db, MAIN, "merge world w", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(leads.find_one(doc! { "_id": 20 }).run().unwrap().unwrap().get_str("owner").ok(), Some("ada"));
    let own = chronos::attach::follow(&db).unwrap();
    assert_eq!(own.changed(), 0, "{own:?}");
    // a world forked before a change followed since conflicts on that document, as after a refresh
    run(&db, MAIN, "create world late", &[]).unwrap();
    run(&db, "late", "update leads set doc = jsonb_set(doc, '{owner}', '\"mine\"') where _id = '20'", &[]).unwrap();
    leads.update_one(doc! { "_id": 20 }, doc! { "$set": { "owner": "theirs" } }).run().unwrap();
    assert_eq!(chronos::attach::follow(&db).unwrap().changed(), 1);
    let e = run(&db, MAIN, "merge world late", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert_eq!(leads.find_one(doc! { "_id": 20 }).run().unwrap().unwrap().get_str("owner").ok(), Some("theirs"));

    // a collection dropped: compared whole (its documents are gone from main)
    notes.drop().run().unwrap();
    let dropped = chronos::attach::follow(&db).unwrap();
    assert_eq!(of(&dropped, "notes"), Some((0, 0, 2)), "{dropped:?}");
    assert_eq!(cell(&db, MAIN, "select count(*) from notes"), "0");
    // (its stream ended there: it follows on from a new place, a collection of that name made again included)
    assert!(dropped.slot.as_deref().is_some_and(|s| s.contains("notes (again")), "{dropped:?}");
    notes.insert_one(doc! { "_id": 9, "org_id": 42, "body": "back" }).run().unwrap();
    let back = chronos::attach::follow(&db).unwrap();
    assert_eq!((of(&back, "notes"), back.slot.clone()), (Some((1, 0, 0)), None), "{back:?}");

    // the place forgotten (`refresh --unfollow`): changes meanwhile are taken by the refresh the next follow begins with
    assert!(chronos::attach::unfollow(&db).unwrap());
    assert!(!chronos::attach::unfollow(&db).unwrap() && !place.exists());
    leads.update_one(doc! { "_id": 20 }, doc! { "$set": { "owner": "while away" } }).run().unwrap();
    let again = chronos::attach::follow(&db).unwrap();
    assert!(again.slot.is_some() && of(&again, "leads") == Some((0, 1, 0)), "{again:?}");
    // a place MongoDB can't read is begun again too, not an error for ever
    std::fs::write(&place, r#"{"tokens":{"leads":"00"},"at":1}"#).unwrap();
    assert!(chronos::attach::follow(&db).unwrap().slot.is_some());
    assert_eq!(chronos::attach::follow(&db).unwrap().changed(), 0);

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    mdb.drop().run().unwrap();
}
