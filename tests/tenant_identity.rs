//! Every world has one tenant identity, set once when it is made (CHRONOS-PLAN-2026-11-17-r3.md
//! section 3, r3.1 section 1): `tenant(column = value)`, `GLOBAL` (explicitly none: main, and worlds
//! people make without a tenant) or `UNASSIGNED` (worlds from before the security release only). A
//! tenant agent acts only on worlds of its tenant, and reads main through its tenant's rows; no
//! fallback ever lends a world the acting agent's or its owner's tenant; nobody changes a world's
//! tenant, admin or not, except `ASSIGN WORLD` taking an `UNASSIGNED` world to a tenant once every
//! check passes.
//!
//! Added failing in batch S0; each `#[ignore]` names the batch that makes it pass.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};
use std::path::{Path, PathBuf};

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create table leads (id int primary key, org_id int not null, name text, owner text);
         create table plans (id int primary key, title text);
         insert into leads values (1, 42, 'acme', 'bo'), (2, 42, 'initech', 'bo'), (3, 7, 'globex', 'eve'),
                                  (4, 7, 'umbrella', 'eve');
         insert into plans values (1, 'free'), (2, 'pro');
         create agent run42 with (can = 'read,fork,write_own,write,merge_own,merge,write_main', tenant = 'org_id = 42');
         create agent run7 with (can = 'read,fork,write_own,merge_own,write_main', tenant = 'org_id = 7');
         create agent plain with (can = 'read,fork,write_own,merge_own,write_main')",
        &[],
    )
    .unwrap();
}

fn agent(name: &str) -> Session {
    let mut s = Session::new(MAIN);
    s.agent = Some(name.into());
    s
}

/// The statement's error code, or "ok".
fn code(db: &Db, s: &mut Session, sql: &str) -> String {
    match s.run(db, sql, &[]) {
        Ok(_) => "ok".into(),
        Err(e) => {
            eprintln!("{sql}: {} {}", e.code, e.message);
            e.code.to_string()
        }
    }
}

fn person() -> Session {
    Session::new(MAIN)
}

fn tenant7_in_main(db: &Db) -> String {
    let out = person().run(db, "select * from leads where org_id = 7 order by id", &[]).unwrap();
    out[0]
        .rows
        .iter()
        .map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The world's row in SHOW WORLDS, by column name.
fn shown_world(db: &Db, w: &str, column: &str) -> String {
    let out = person().run(db, "show worlds", &[]).unwrap().pop().unwrap();
    let i = out
        .columns
        .as_ref()
        .unwrap()
        .iter()
        .position(|(n, _)| n == column)
        .unwrap_or_else(|| panic!("no column {column}"));
    let row = out.rows.iter().find(|r| r[0].to_text() == w).unwrap_or_else(|| panic!("no world {w}"));
    row[i].to_text()
}

/// Every verb T4 names, as `agent` on world `w`: each must be refused (42501).
fn every_verb_refused(db: &Db, agent_name: &str, w: &str) {
    let mut s = agent(agent_name);
    for sql in [
        format!("use world {w}"),
        format!("diff world {w}"),
        format!("fork world {w} as {w}_child"),
        format!("merge world {w}"),
        format!("merge world {w} dry run"),
        format!("rebase world {w}"),
        format!("drop world {w}"),
        format!("alter world {w} set (note = 'x')"),
        format!("replay world {w}"),
    ] {
        assert_eq!(code(db, &mut s, &sql), "42501", "{agent_name}: {sql}");
    }
    let mut inside = Session::new(w);
    inside.agent = Some(agent_name.into());
    for sql in ["select * from leads", "update leads set owner = 'x' where id = 1", "select * from plans"] {
        assert_eq!(code(db, &mut inside, sql), "42501", "{agent_name} in {w}: {sql}");
    }
    assert!(db.world(w).is_ok(), "{w} is still there");
}

#[test]
fn unassigned_world_refused_for_tenant_agent() {
    // UNASSIGNED worlds exist only from before the security release: the 0.5.5 fixture's z_0
    let dir = legacy("unassigned");
    let db = Db::open(&dir).unwrap();
    // refused to its owner (legacy_fixture_quarantine) and to any other tenant agent
    run(
        &db,
        MAIN,
        "create agent other42 with (can = 'read,fork,write_own,merge_own,write_main,merge', tenant = 'org_id = 42')",
        &[],
    )
    .unwrap();
    every_verb_refused(&db, "other42", "z_0");
}

#[test]
fn global_world_refused_for_tenant_agent() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create world shared_w", &[]).unwrap(); // a person's world, no tenant: GLOBAL
    agent("plain").run(&db, "create world plain_w", &[]).unwrap(); // a plain agent's: GLOBAL
    every_verb_refused(&db, "run42", "shared_w");
    every_verb_refused(&db, "run42", "plain_w");
}

#[test]
fn other_tenant_world_refused() {
    let db = Db::new();
    setup(&db);
    agent("run7").run(&db, "create world w7", &[]).unwrap();
    run(&db, MAIN, "create world p7 with (tenant = 'org_id = 7')", &[]).unwrap();
    every_verb_refused(&db, "run42", "w7");
    every_verb_refused(&db, "run42", "p7");
}

/// T5: a world without a tenant is not lent the acting agent's tenant: a tenant agent holding the
/// `merge` right still may not merge a person's world, even one whose changes are all its tenant's.
#[test]
fn no_fallback_to_acting_agent_tenant() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create world pw; use world pw; update leads set owner = 'p' where id = 1", &[]).unwrap();
    let mut s = agent("run42");
    assert_eq!(code(&db, &mut s, "merge world pw"), "42501");
    assert_eq!(person().run(&db, "select owner from leads where id = 1", &[]).unwrap()[0].rows[0][0].to_text(), "bo");
    // and the world is still GLOBAL, not tenant 42's
    assert!(db.world("pw").unwrap().scope.and_then(|s| s.get("tenant").cloned()).is_none());
}

/// T5: a world a tenant agent owns but that records no tenant (only possible before the security
/// release) is not lent its owner's tenant either: it is UNASSIGNED and quarantined.
#[test]
fn no_fallback_to_owner_tenant() {
    let dir = legacy("owner_fallback");
    let db = Db::open(&dir).unwrap();
    assert!(db.world("z_0").unwrap().scope.and_then(|s| s.get("tenant").cloned()).is_none());
    assert_eq!(shown_world(&db, "z_0", "tenant_status"), "quarantined");
    let mut s = agent("run42"); // its owner
    assert_eq!(code(&db, &mut s, "merge world z_0"), "42501");
}

/// T2: no statement changes a world's tenant after it is made, admin or not.
#[test]
fn tenant_immutable_even_for_admin() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create agent boss with (can = 'admin')", &[]).unwrap();
    agent("run42").run(&db, "create world mine", &[]).unwrap();
    run(&db, MAIN, "create world pw with (tenant = 'org_id = 42')", &[]).unwrap();
    run(&db, MAIN, "create world gw", &[]).unwrap();
    for (who, mut s) in [("person", person()), ("admin agent", agent("boss"))] {
        for (w, to) in [
            ("mine", "tenant = 'org_id = 7'"),
            ("mine", "tenant = null"),
            ("pw", "tenant = 'org_id = 7'"),
            ("gw", "tenant = 'org_id = 42'"),
        ] {
            assert_eq!(code(&db, &mut s, &format!("alter world {w} set ({to})")), "42501", "{who}: {w} {to}");
        }
        // narrowing what it may change is still allowed
        assert_eq!(code(&db, &mut s, "alter world pw set (may_change = 'leads.owner')"), "ok", "{who}");
    }
    assert_eq!(db.world("mine").unwrap().scope.unwrap()["tenant"]["value"], 42);
    assert_eq!(db.world("pw").unwrap().scope.unwrap()["tenant"]["value"], 42);
}

/// T1: a child world gets its parent's identity and may not declare another.
#[test]
fn child_world_inherits_identity() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create world pw with (tenant = 'org_id = 42')", &[]).unwrap();
    run(&db, MAIN, "fork world pw as pw_child", &[]).unwrap();
    assert_eq!(db.world("pw_child").unwrap().scope.unwrap()["tenant"]["value"], 42);
    assert_eq!(code(&db, &mut person(), "fork world pw as pw_other with (tenant = 'org_id = 7')"), "42501");
    run(&db, MAIN, "create world gw", &[]).unwrap();
    assert_eq!(code(&db, &mut person(), "fork world gw as gw_t with (tenant = 'org_id = 7')"), "42501");
}

fn assign(db: &Db, w: &str) -> String {
    code(db, &mut person(), &format!("assign world {w} to tenant 'org_id = 42' reason 'checked by hand'"))
}

/// Section 3.3: each of the six checks refuses on its own. `tests/fixtures/legacy-assign` (made by
/// the 0.5.5 binary) holds kept simulation worlds of tenant agent run42, none with a tenant in its
/// scope, each with one offence: `clean_0` (a person changed a tenant-42 row: passes every check),
/// `stray_0` (a person changed a tenant-7 row: check 2), `ddl_0` (its script added a column to
/// `leads`: check 3), `grants_0` (a person made a role and granted it: check 4), `parent_0` with a
/// person's fork `child_x` that changed a tenant-7 row (check 5), and `past_0` (simulated from a
/// past moment: check 6, which passes here since main hasn't changed since: its rows put back are none).
/// `legacy-sim-kept`'s `z_0` fails checks 2 and 3 together (finding A).
#[test]
fn assign_checks() {
    // 1. only an UNASSIGNED world: a GLOBAL or tenant world is refused
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create world gw; create world tw with (tenant = 'org_id = 42')", &[]).unwrap();
    assert_ne!(assign(&db, "gw"), "ok", "check 1: GLOBAL");
    assert_ne!(assign(&db, "tw"), "ok", "check 1: already a tenant's");
    assert_ne!(assign(&db, MAIN), "ok", "check 1: main");
    // only a person or an admin may assign
    assert_eq!(code(&db, &mut agent("run42"), "assign world gw to tenant 'org_id = 42' reason 'x'"), "42501");
    drop(db);

    let dir = fixture("legacy-assign", "assign_checks");
    let db = Db::open(&dir).unwrap();
    let main_before = tenant7_in_main(&db);
    for (w, check) in [
        ("stray_0", "2: a row of another tenant"),
        ("ddl_0", "3: a held table's definition"),
        ("grants_0", "4: grant rows"),
        ("parent_0", "5: a descendant fails"),
    ] {
        assert_ne!(assign(&db, w), "ok", "check {check}");
        assert_eq!(shown_world(&db, w, "tenant_status"), "quarantined", "{w}: a refused assign changes nothing");
    }
    // check 5 assigns all or none: the child was not assigned either
    assert_eq!(shown_world(&db, "child_x", "tenant_status"), "unassigned");
    assert_eq!(tenant7_in_main(&db), main_before);
    drop(db);

    let dir = legacy("assign_checks_z");
    let db = Db::open(&dir).unwrap();
    assert_ne!(assign(&db, "z_0"), "ok", "checks 2 and 3: the kept exploit world");
}

/// After a successful assignment the world is its tenant's: its owner may merge it, and the merge
/// is held to that tenant's rows. One way: it can't be assigned again.
#[test]
fn assign_success_then_merge_held_to_tenant() {
    let dir = fixture("legacy-assign", "assign_ok");
    let db = Db::open(&dir).unwrap();
    assert_eq!(shown_world(&db, "clean_0", "tenant_status"), "quarantined");
    assert_eq!(assign(&db, "clean_0"), "ok");
    assert_eq!(db.world("clean_0").unwrap().scope.unwrap()["tenant"]["value"], 42);
    assert_eq!(shown_world(&db, "clean_0", "tenant_status"), "tenant");
    assert_ne!(assign(&db, "clean_0"), "ok");
    let before = tenant7_in_main(&db);
    agent("run42").run(&db, "merge world clean_0", &[]).unwrap();
    assert_eq!(tenant7_in_main(&db), before);
    let owner = person().run(&db, "select owner from leads where id = 1", &[]).unwrap()[0].rows[0][0].to_text();
    assert_eq!(owner, "person");
    // the assignment is in the security audit, with who and why
    let h = person().run(&db, "show security audit", &[]).unwrap().pop().unwrap();
    let text: Vec<String> =
        h.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")).collect();
    assert!(text.iter().any(|l| l.contains("assign") && l.contains("clean_0")), "{text:#?}");
}

/// T7: an agent's tenant is fixed while it owns worlds.
#[test]
fn agent_tenant_fixed_while_owning_worlds() {
    let db = Db::new();
    setup(&db);
    agent("run42").run(&db, "create world mine", &[]).unwrap();
    for t in ["'org_id = 7'", "null"] {
        assert_eq!(code(&db, &mut person(), &format!("alter agent run42 set (tenant = {t})")), "42501", "{t}");
    }
    run(&db, MAIN, "drop world mine", &[]).unwrap();
    assert_eq!(code(&db, &mut person(), "alter agent run42 set (tenant = 'org_id = 7')"), "ok");
}

/// T6: a merge of a tenant world is held to its tenant whoever merges it, including an actor under
/// a merge policy, where a stray row is today a reason to hold for review rather than a refusal.
#[test]
fn policy_actor_stray_rows_refused() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "create merge policy p", &[]).unwrap();
    run(&db, MAIN, "create agent mgr with (can = 'read,fork,write,merge,write_main', policy = 'p')", &[]).unwrap();
    run(&db, MAIN, "create world tw with (tenant = 'org_id = 42')", &[]).unwrap();
    // a person writes a tenant-7 row into the tenant-42 world (people aren't filtered)
    Session::new("tw").run(&db, "update leads set owner = 'stray' where id = 3", &[]).unwrap();
    let before = tenant7_in_main(&db);
    for (who, mut s) in [("policy agent", agent("mgr")), ("person", person())] {
        let e = s.run(&db, "merge world tw", &[]).unwrap_err();
        assert_eq!(e.code, "42501", "{who}: {}", e.message);
        assert_eq!(tenant7_in_main(&db), before, "{who}");
        // and not queued for review either
        let reviews = person().run(&db, "show reviews", &[]).unwrap().pop().unwrap();
        assert!(reviews.rows.iter().all(|r| r[0].to_text() != "tw"), "{who}: queued");
    }
}

// --- Legacy worlds (r3 section 3.4) ---

/// A copy of a 0.5.5 fixture folder (tests/fixtures/<name>), made by the 0.5.5 release binary:
/// `legacy-sim-kept` holds tenant agent run42's kept, unscoped simulation world z_0 whose script
/// rewrote `leads.org_id` to 42 (finding B), not merged; `legacy-sim-merged` holds the same after
/// `merge world z_0` (main's tenant-7 row now reads org_id 42).
fn fixture(name: &str, tag: &str) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let dir = std::env::temp_dir().join(format!("chronos-tenant-identity-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&from, &dir);
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_type().unwrap().is_dir() {
            copy_dir(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

fn legacy(tag: &str) -> PathBuf {
    fixture("legacy-sim-kept", tag)
}

#[test]
fn legacy_fixture_quarantine() {
    let dir = legacy("quarantine");
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.world("z_0").unwrap().owner.as_deref(), Some("run42"));
    assert_eq!(shown_world(&db, "z_0", "tenant_status"), "quarantined");
    // its owner may not merge, read or fork it
    every_verb_refused(&db, "run42", "z_0");
    // a person may inspect it
    assert!(person().run(&db, "diff world z_0", &[]).is_ok());
    // and may not assign it while it holds stray rows (finding A rewrote tenant 7's row)
    assert_ne!(assign(&db, "z_0"), "ok");
    // the folder is not rewritten by opening it: identity is derived
    drop(db);
    let manifest = std::fs::read(dir.join("manifest")).unwrap();
    let original =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-sim-kept/manifest")).unwrap();
    assert_eq!(manifest.first(), original.first(), "manifest version unchanged");
}

/// PR #362 review: a world forked from an unassigned world holds what that world changed, though its own diff is
/// empty. Its changes are counted from where the line of unassigned worlds left main, so it is refused as its parent
/// is.
#[test]
fn assign_counts_what_an_unassigned_parent_changed() {
    let dir = fixture("legacy-assign", "assign_inherited");
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "fork world stray_0 as stray_kid", &[]).unwrap();
    assert_eq!(shown_world(&db, "stray_kid", "tenant_status"), "unassigned");
    assert!(db.diff("stray_kid").unwrap().is_empty(), "the child changed nothing itself");
    let e =
        person().run(&db, "assign world stray_kid to tenant 'org_id = 42' reason 'checked by hand'", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("stray_kid"), "{}", e.message);
    assert_eq!(shown_world(&db, "stray_kid", "tenant_status"), "unassigned");
    // a clean parent's child still passes
    run(&db, MAIN, "fork world clean_0 as clean_kid", &[]).unwrap();
    assert_eq!(assign(&db, "clean_kid"), "ok");
}

/// PR #362 review: every world's new metadata is checked against the limit before any is written, so a descendant
/// that can't take the assignment's record leaves the whole tree unassigned.
#[test]
fn assign_is_all_or_none_when_a_record_does_not_fit() {
    let dir = fixture("legacy-assign", "assign_meta_full");
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "fork world clean_0 as full_kid", &[]).unwrap();
    // fill the child's metadata to within a few bytes of the limit, with what only its stored metadata shows (its
    // scope's intent: not in `World::meta`)
    let mut n = 64 << 10;
    while run(&db, MAIN, &format!("alter world full_kid set (intent = '{}')", "x".repeat(n)), &[]).is_err() {
        n -= 16;
        assert!(n > 1024, "the intent was never taken");
    }
    assert!(!db.world("full_kid").unwrap().meta.to_string().contains("xxxx"), "the intent is internal");
    assert_ne!(assign(&db, "clean_0"), "ok");
    assert_eq!(shown_world(&db, "clean_0", "tenant_status"), "quarantined", "the root was not assigned");
    assert_eq!(shown_world(&db, "full_kid", "tenant_status"), "unassigned");
    assert!(db.world("clean_0").unwrap().scope.is_none_or(|s| s.get("tenant").is_none_or(|t| t.is_null())));
}

/// PR #362 review: two ASSIGN WORLDs of one world at once: one wins, the other is refused, and the world holds
/// one assignment.
#[test]
fn concurrent_assigns_assign_once() {
    for i in 0..5 {
        let dir = fixture("legacy-assign", &format!("assign_race_{i}"));
        let db = std::sync::Arc::new(Db::open(&dir).unwrap());
        let go = std::sync::Arc::new(std::sync::Barrier::new(2));
        let racer = |sql: &'static str| {
            let (db, go) = (db.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                person().run(&db, sql, &[]).is_ok()
            })
        };
        let a = racer("assign world clean_0 to tenant 'org_id = 42' reason 'race a'");
        let b = racer("assign world clean_0 to global reason 'race b'");
        let (a, b) = (a.join().unwrap(), b.join().unwrap());
        assert!(a ^ b, "exactly one wins: {a} {b}");
        let status = shown_world(&db, "clean_0", "tenant_status");
        assert_eq!(status, if a { "tenant" } else { "global" }, "the winner's identity");
        let assigned = db.security_events(100).into_iter().filter(|e| e.action == "assign world").collect::<Vec<_>>();
        assert_eq!(
            assigned.len(),
            1,
            "one assignment recorded: {:?}",
            assigned.iter().map(|e| &e.target).collect::<Vec<_>>()
        );
        assert!(assigned[0].target.contains(if a { "race a" } else { "race b" }), "{}", assigned[0].target);
    }
}

/// TODOS (PR #362 review): a fork or a discard of a world in the tree, racing ASSIGN WORLD, can't leave the root
/// assigned with a descendant not. Whatever the timing: assigned, every world of the tree is the tenant's (a fork
/// made after it inherits the tenant); refused, the root is as it was.
#[test]
fn assign_is_all_or_none_against_forks_and_discards() {
    for i in 0..5 {
        let dir = fixture("legacy-assign", &format!("assign_vs_tree_{i}"));
        let db = std::sync::Arc::new(Db::open(&dir).unwrap());
        run(&db, MAIN, "fork world clean_0 as kid_a", &[]).unwrap();
        let go = std::sync::Arc::new(std::sync::Barrier::new(2));
        let forker = {
            let (db, go) = (db.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                for n in 0..20 {
                    let _ = run(&db, MAIN, &format!("fork world kid_a as kid_{n}"), &[]);
                    if n % 2 == 0 {
                        let _ = run(&db, MAIN, &format!("drop world kid_{n}"), &[]);
                    }
                }
            })
        };
        let assigner = {
            let (db, go) = (db.clone(), go.clone());
            std::thread::spawn(move || {
                go.wait();
                person().run(&db, "assign world clean_0 to tenant 'org_id = 42' reason 'race'", &[]).map_err(|e| e.code)
            })
        };
        let assigned = assigner.join().unwrap();
        forker.join().unwrap();
        let root = shown_world(&db, "clean_0", "tenant_status");
        let tree: Vec<(String, String)> =
            db.descendants("clean_0").into_iter().map(|w| (shown_world(&db, &w, "tenant_status"), w)).collect();
        match assigned {
            Ok(_) => {
                assert_eq!(root, "tenant");
                assert!(tree.iter().all(|(s, _)| s == "tenant"), "run {i}: the whole tree: {tree:?}");
            }
            // (40001 from the writes; a world of the tree gone while its checks ran says so as any missing world)
            Err(code) => {
                assert_ne!(code, "42501", "run {i}: refused by a check: the tree is clean");
                assert_eq!(root, "quarantined", "run {i}: the root as it was");
                assert!(tree.iter().all(|(s, _)| s == "unassigned"), "run {i}: none of the tree: {tree:?}");
            }
        }
    }
}

/// The same ASSIGN WORLD (tenant and reason) run again after a crash cut it short finishes the worlds it left, and
/// once more does nothing; a world another ASSIGN WORLD gave the tenant still refuses it, as it always did.
#[test]
fn assign_run_again_finishes_only_the_same_statement() {
    let dir = fixture("legacy-assign", "assign_again");
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "fork world clean_0 as kid", &[]).unwrap();
    // as a crash partway through the log leaves it: the descendant written, the root not
    assert_eq!(assign(&db, "kid"), "ok");
    assert_eq!(shown_world(&db, "clean_0", "tenant_status"), "quarantined");
    // a fork of it made since inherits its tenant, with no record of its own: reached too
    run(&db, MAIN, "fork world kid as grandkid", &[]).unwrap();
    assert_eq!(shown_world(&db, "grandkid", "tenant_status"), "tenant");
    assert_eq!(assign(&db, "clean_0"), "ok", "the same statement finishes the tree");
    assert_eq!(shown_world(&db, "clean_0", "tenant_status"), "tenant");
    assert_eq!(shown_world(&db, "grandkid", "tenant_status"), "tenant");
    assert_ne!(assign(&db, "clean_0"), "ok", "once the root is assigned, it can't be assigned again");
    let assigned = db.security_events(100).into_iter().filter(|e| e.action == "assign world").count();
    assert_eq!(assigned, 2, "kid once, clean_0 once");
    drop(db);

    let dir = fixture("legacy-assign", "assign_other");
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "fork world clean_0 as kid", &[]).unwrap();
    run(&db, MAIN, "assign world kid to tenant 'org_id = 42' reason 'another'", &[]).unwrap();
    assert_ne!(assign(&db, "clean_0"), "ok", "a world another statement assigned: refused");
    assert_eq!(shown_world(&db, "clean_0", "tenant_status"), "quarantined");
}
