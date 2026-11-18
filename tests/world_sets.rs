//! World sets: worlds forked from one base, run, compared and ranked as a group, the best merged into the base and
//! the rest discarded; objectives rank them by one query and the merge checks they must pass.

use chronos::sql::{self, Output, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, world: &str, q: &str) -> Output {
    sql::run(db, world, q, &[]).unwrap_or_else(|e| panic!("{q}: {e:?}")).pop().unwrap()
}

fn col(o: &Output, i: usize) -> Vec<String> {
    o.rows.iter().map(|r| r[i].to_text()).collect()
}

/// `n` accounts of 100 each, and a base world `plan` forked from main.
fn setup(db: &Db, n: usize) {
    run(db, MAIN, "create table accounts (id bigint primary key, bal double precision)");
    run(db, MAIN, &format!("insert into accounts select g, 100 from generate_series(1, {n}) g"));
    db.fork(MAIN, "plan").unwrap();
}

const SCRIPT: &str = "$$update accounts set bal = bal + (random() - 0.5) * 10 + $1$$";

#[test]
fn fork_world_set_tags_members_and_is_deterministic() {
    // on disk: REPLAY WORLD reads the base as it was
    let dir = std::env::temp_dir().join(format!("chronos-worldsets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    setup(&db, 200);
    // a set's base is never main
    let e = sql::run(&db, MAIN, "create world set m from world main", &[]).unwrap_err();
    assert_eq!(e.code, "0A000");
    run(&db, MAIN, "create world set a from world plan with (max_live = 6)");
    run(&db, MAIN, "create world set b from world plan");
    assert_eq!(sql::run(&db, MAIN, "create world set a from world plan", &[]).unwrap_err().code, "42710");
    let fa = run(&db, MAIN, &format!("fork world set a count 4 run {SCRIPT} seed 42 threads 1"));
    let names: Vec<&str> = fa.columns.as_ref().unwrap().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["world", "id", "index", "seed", "error"]);
    assert_eq!(fa.tag, "FORK WORLD SET 4");
    assert_eq!(col(&fa, 0), ["a_0", "a_1", "a_2", "a_3"]);
    run(&db, MAIN, &format!("fork world set b count 4 run {SCRIPT} seed 42 threads 4"));
    for i in 0..4 {
        let w = db.world(&format!("a_{i}")).unwrap();
        assert_eq!(w.parent.as_deref(), Some("plan"), "members are forks of the base");
        assert_eq!(w.meta["_sonos_set"], "a");
        assert!(w.meta["sim"].is_object(), "what made it is kept, for REPLAY");
        let d = db.diff_between(&format!("a_{i}"), &format!("b_{i}")).unwrap();
        assert!(d.is_empty(), "same seed, 1 thread or 4: same rows in world {i}");
    }
    assert!(!db.diff_between("a_0", "a_1").unwrap().is_empty(), "each member has its own numbers");
    // indexes carry on; max_live holds
    let more = run(&db, MAIN, &format!("fork world set a count 2 run {SCRIPT} seed 42"));
    assert_eq!(col(&more, 0), ["a_4", "a_5"]);
    let e = sql::run(&db, MAIN, &format!("fork world set a count 1 run {SCRIPT}"), &[]).unwrap_err();
    assert_eq!(e.code, "53400", "{e:?}");
    // a member is replayable as a simulation's world is
    let r = run(&db, MAIN, "replay world a_2");
    assert_eq!(r.rows[0][2], Value::Bool(true), "{:?}", r.rows);
    // the script can't leave its world
    let e = sql::run(&db, MAIN, "fork world set b count 1 run $$merge world plan$$", &[]).unwrap_err();
    assert_eq!(e.code, "0A000");
    // ADOPT: a fork of the base joins; anything else is refused
    db.fork("plan", "mine").unwrap();
    run(&db, MAIN, "adopt world mine into set b");
    assert_eq!(db.world("mine").unwrap().meta["_sonos_set"], "b");
    db.fork(MAIN, "stranger").unwrap();
    assert_eq!(sql::run(&db, MAIN, "adopt world stranger into set b", &[]).unwrap_err().code, "55000");
    assert_eq!(sql::run(&db, MAIN, "adopt world mine into set a", &[]).unwrap_err().code, "55000");
    // not inside a transaction
    let mut s = sql::Session::new(MAIN);
    s.run(&db, "begin", &[]).unwrap();
    assert_eq!(s.run(&db, "rank world set a by ($$select 1$$)", &[]).unwrap_err().code, "25001");
    s.run(&db, "rollback", &[]).unwrap();
    s.close(&db);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compare_worlds_matches_diff() {
    let db = Db::new();
    setup(&db, 50);
    run(&db, MAIN, "create world set s from world plan");
    run(&db, MAIN, "fork world set s count 2 run $$update accounts set bal = bal + $1 where id % 7 = $1$$");
    run(&db, "s_0", "insert into accounts values (1000, 1)");
    run(&db, "s_1", "delete from accounts where id = 3");
    let want: Vec<(String, Option<String>, Option<String>)> = db
        .diff_between("s_0", "s_1")
        .unwrap()
        .into_iter()
        .map(|c| (c.key, c.before.map(|v| v.to_string()), c.after.map(|v| v.to_string())))
        .collect();
    assert!(!want.is_empty());
    let c = db.compare_worlds("s_0", "s_1", None).unwrap();
    assert!(c.changed.is_some(), "siblings: compared by their changes");
    let got: Vec<_> = c.rows.into_iter().map(|d| (d.key, d.a, d.b)).collect();
    assert_eq!(got, want);
    // over SQL: table and key apart
    let o = run(&db, MAIN, "compare worlds s_0, s_1");
    assert_eq!(o.rows.len(), want.len());
    assert!(o.rows.iter().all(|r| r[0] == Value::Text("accounts".into())));
    // the row only s_0 has, and the one only s_1 lacks
    assert!(o.rows.iter().any(|r| r[2] != Value::Null && r[3] == Value::Null));
    let sum = run(&db, MAIN, "compare worlds s_0, s_1 summary");
    assert_eq!(col(&sum, 0), ["accounts"]);
    assert_eq!(sum.rows[0][3], Value::Int(want.len() as i64));
    // worlds with no fork point in common: row by row, the same answer
    db.fork(MAIN, "far").unwrap();
    let want: Vec<String> = db.diff_between("s_0", "far").unwrap().into_iter().map(|c| c.key).collect();
    let c = db.compare_worlds("s_0", "far", None).unwrap();
    assert!(c.changed.is_none());
    assert_eq!(c.rows.into_iter().map(|d| d.key).collect::<Vec<_>>(), want);
}

/// A set `s` of 4 members whose total balance is 100 * 20 + 10 * index, and member 2 overdrawn.
fn ranked_set(db: &Db) {
    setup(db, 20);
    run(db, MAIN, "create world set s from world plan");
    run(db, MAIN, "fork world set s count 4 run $$update accounts set bal = bal + 10 * $1 where id = 1$$");
    run(db, "s_2", "update accounts set bal = -5 where id = 2");
    db.create_check("no_overdraft", "select id from accounts where bal < 0", &["accounts".into()], None, false)
        .unwrap();
}

#[test]
fn rank_orders_by_metric_and_failed_last() {
    let db = Db::new();
    ranked_set(&db);
    let o = run(&db, MAIN, "rank world set s by ($$select sum(bal) from accounts$$ desc) with checks");
    assert_eq!(o.tag, "RANK 4");
    // s_2 breaks the check: last
    assert_eq!(col(&o, 0), ["s_3", "s_1", "s_0", "s_2"]);
    assert_eq!(col(&o, 1), ["1", "2", "3", "4"]);
    assert_eq!(o.rows[3][4], Value::Bool(true));
    assert!(o.rows[3][5].to_text().contains("no_overdraft"), "{:?}", o.rows[3]);
    assert_eq!(o.rows[0][2], Value::Float(2030.0));
    // without checks, by the metric alone; ASC reverses it; a second metric breaks ties
    let o = run(&db, MAIN, "rank world set s by ($$select sum(bal) from accounts$$ asc)");
    assert_eq!(col(&o, 0), ["s_2", "s_0", "s_1", "s_3"]);
    let o = run(&db, MAIN, "rank world set s by ($$select 1$$, $$select max(bal) from accounts$$ desc)");
    assert_eq!(col(&o, 0), ["s_3", "s_2", "s_1", "s_0"]);
    // a metric that fails ranks last; one that writes is refused
    let o = run(&db, MAIN, "rank world set s by ($$select 1 / (sum(bal)::bigint - 2030) from accounts$$)");
    assert_eq!(col(&o, 0).last().unwrap(), "s_3");
    assert_eq!(o.rows[3][4], Value::Bool(true));
    let e = sql::run(&db, MAIN, "rank world set s by ($$delete from accounts$$)", &[]).unwrap_err();
    assert_eq!(e.code, "22023");
    // the ranking is kept in each member's metadata
    assert_eq!(db.world("s_3").unwrap().meta["_sonos_set_scores"]["rank"], 4);
    assert_eq!(db.world("s_3").unwrap().meta["_sonos_set_scores"]["failed"], true);
}

#[test]
fn prune_discards() {
    let db = Db::new();
    ranked_set(&db);
    // nothing ranked yet: refused
    assert_eq!(sql::run(&db, MAIN, "prune world set s where failed", &[]).unwrap_err().code, "55000");
    run(&db, MAIN, "rank world set s by ($$select sum(bal) from accounts$$) with checks (no_overdraft)");
    let o = run(&db, MAIN, "prune world set s where failed");
    assert_eq!(col(&o, 0), ["s_2"]);
    assert!(db.world("s_2").is_err());
    let o = run(&db, MAIN, "prune world set s where rank > 2 or failed");
    assert_eq!(col(&o, 0), ["s_0"]);
    let (_, left) = db.world_set("s").unwrap();
    assert_eq!(left, ["s_1", "s_3"]);
    // a member changed since its ranking: rank again first
    run(&db, "s_1", "update accounts set bal = 0 where id = 5");
    assert_eq!(sql::run(&db, MAIN, "prune world set s where rank > 1", &[]).unwrap_err().code, "40001");
    assert!(db.world("plan").is_ok(), "the base stays");
}

#[test]
fn merge_best_merges_winner_into_base_only() {
    let db = Db::new();
    ranked_set(&db);
    run(&db, MAIN, "rank world set s by ($$select sum(bal) from accounts$$) with checks");
    // only into the base
    let e = sql::run(&db, MAIN, "merge best of set s into main", &[]).unwrap_err();
    assert_eq!(e.code, "0A000");
    let dry = run(&db, MAIN, "merge best of set s into plan dry run");
    assert_eq!(dry.rows[0][0..2], [Value::Text("s_3".into()), Value::Text("would merge".into())]);
    assert_eq!(dry.rows.len(), 4);
    assert!(db.world("s_0").is_ok(), "a dry run changes nothing");
    let o = run(&db, MAIN, "merge best of set s into plan");
    assert_eq!(o.rows[0][0..2], [Value::Text("s_3".into()), Value::Text("merged".into())]);
    assert!(o.rows[1..].iter().all(|r| r[1] == Value::Text("discarded".into())), "{:?}", o.rows);
    for w in ["s_0", "s_1", "s_2"] {
        assert!(db.world(w).is_err(), "{w} discarded");
    }
    // the base has the winner's rows; main has nothing of them
    let plan = run(&db, "plan", "select bal from accounts where id = 1");
    assert_eq!(plan.rows[0][0], Value::Float(130.0));
    let main = run(&db, MAIN, "select bal from accounts where id = 1");
    assert_eq!(main.rows[0][0], Value::Float(100.0));
}

#[test]
fn optimize_is_deterministic_across_threads() {
    let db = Db::new();
    setup(&db, 300);
    db.create_check("no_overdraft", "select id from accounts where bal < 0", &["accounts".into()], None, false)
        .unwrap();
    run(
        &db,
        MAIN,
        "create objective spend minimize $$select sum(bal) from accounts$$ subject to checks (no_overdraft)",
    );
    assert_eq!(
        sql::run(&db, MAIN, "create objective bad maximize $$select 1$$ subject to checks (nope)", &[])
            .unwrap_err()
            .code,
        "42704"
    );
    // each world moves a fifth of the balances down by a seeded random amount; every third world overdraws
    let script = "$$update accounts set bal = bal - random() * 50 - case when $1 % 3 = 0 then 200 else 0 end \
                  where id % 5 = $1 % 5$$";
    let go = |set: &str, threads: usize| {
        run(
            &db,
            MAIN,
            &format!(
                "optimize spend from world plan as set {set} run {script} count 8 keep 3 seed 7 threads {threads}"
            ),
        )
    };
    let a = go("one", 1);
    let b = go("many", 6);
    assert_eq!(a.tag, "OPTIMIZE 8");
    let key = |o: &Output| -> Vec<(String, String, String, String)> {
        o.rows
            .iter()
            .map(|r| {
                (
                    r[0].to_text().split('_').next_back().unwrap().to_string(),
                    r[1].to_text(),
                    r[2].to_text(),
                    r[6].to_text(),
                )
            })
            .collect()
    };
    assert_eq!(key(&a), key(&b), "same ranking on 1 thread or 6");
    let kept: Vec<_> = a.rows.iter().filter(|r| r[6] == Value::Bool(true)).collect();
    assert!(kept.len() <= 3 && !kept.is_empty(), "{:?}", a.rows);
    assert!(kept.iter().all(|r| r[4] == Value::Bool(false)), "failed worlds are never kept");
    let (_, left) = db.world_set("one").unwrap();
    assert_eq!(left.len(), kept.len());
    // lowest total first among those that passed
    let scores: Vec<f64> =
        a.rows.iter().filter(|r| r[4] == Value::Bool(false)).map(|r| r[2].to_text().parse().unwrap()).collect();
    assert!(scores.windows(2).all(|w| w[0] <= w[1]), "{scores:?}");
    // agents may not make objectives
    db.create_agent("bot", &chronos::AgentSpec::default()).unwrap();
    let mut s = sql::Session::new(MAIN);
    s.agent = Some("bot".into());
    assert_eq!(s.run(&db, "create objective x maximize $$select 1$$", &[]).unwrap_err().code, "42501");
    s.close(&db);
}

/// RANK ... WITH CHECKS by an agent: a check reads every row, so its reasons come without values, as a merge's do.
#[test]
fn an_agents_rank_shows_check_reasons_without_values() {
    let db = Db::new();
    setup(&db, 3);
    run(&db, MAIN, "insert into accounts values (777, -1)"); // a row the agent's members never touched
    db.create_check("no_overdraft", "select id from accounts where bal < 0", &["accounts".into()], None, false)
        .unwrap();
    db.create_agent("bot", &chronos::AgentSpec::default()).unwrap();
    let mut s = sql::Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(&db, "create world mine", &[]).unwrap();
    s.run(&db, "create world set s from world mine", &[]).unwrap();
    s.run(&db, "fork world set s count 1 run $$update accounts set bal = bal + 1 where id = 1$$", &[]).unwrap();
    let o = s.run(&db, "rank world set s by ($$select 1$$) with checks", &[]).unwrap().pop().unwrap();
    let why = o.rows[0][5].to_text();
    assert!(why.contains("no_overdraft") && !why.contains("777"), "{why}");
    s.close(&db);
    // a person's shows them
    let o = run(&db, MAIN, "rank world set s by ($$select 1$$) with checks");
    assert!(o.rows[0][5].to_text().contains("777"), "{:?}", o.rows[0]);
}

#[test]
fn agent_members_are_forked_as_the_agent_forks() {
    let db = Db::new();
    run(&db, MAIN, "create table leads (id int primary key, org_id int not null, score int)");
    run(&db, MAIN, "insert into leads values (1, 7, 0), (2, 42, 0)");
    let spec = chronos::AgentSpec { tenant: Some(serde_json::json!({"org_id": 7})), ..Default::default() };
    db.create_agent("run7", &spec).unwrap();
    db.as_agent("run7").unwrap().fork(MAIN, "base7", serde_json::Value::Null).unwrap();
    let mut s = sql::Session::new(MAIN);
    s.agent = Some("run7".into());
    s.run(&db, "create world set t from world base7", &[]).unwrap();
    // (a script that writes the tenant's table hits `simulate_as_tenant_agent`'s overflow)
    s.run(&db, "fork world set t count 2 run $$select 1$$ seed 3", &[]).unwrap();
    for w in ["t_0", "t_1"] {
        let w = db.world(w).unwrap();
        assert_eq!(w.owner.as_deref(), Some("run7"), "the agent's own");
        assert_eq!(w.parent.as_deref(), Some("base7"));
        let scope = w.scope.expect("its tenant, as Acting::fork stamps it");
        assert_eq!(scope["tenant"]["value"], 7, "{scope}");
    }
    // what it reads there is its tenant's
    let out = s.run(&db, "select count(*) from leads at world t_1", &[]).unwrap();
    assert_eq!(out[0].rows[0][0], Value::Int(1));
    // an agent may not make a set of a world it can't change
    db.fork(MAIN, "others").unwrap();
    assert_eq!(s.run(&db, "create world set u from world others", &[]).unwrap_err().code, "42501");
    s.close(&db);
}

/// A tenant agent's script run by SIMULATE (`run_world`, with parameters) runs: asking the types of its parameters
/// once overflowed the stack, its tenant's row filter added to the rewritten statement again without end.
#[test]
fn simulate_as_tenant_agent() {
    let db = Db::new();
    run(&db, MAIN, "create table leads (id int primary key, org_id int not null, score int)");
    run(&db, MAIN, "insert into leads values (1, 7, 10), (2, 8, 20)");
    let spec = chronos::AgentSpec { tenant: Some(serde_json::json!({"org_id": 7})), ..Default::default() };
    db.create_agent("run7", &spec).unwrap();
    db.as_agent("run7").unwrap().fork(MAIN, "base7", serde_json::Value::Null).unwrap();
    let mut s = sql::Session::new("base7");
    s.agent = Some("run7".into());
    s.run(
        &db,
        "simulate 2 worlds from base7 as sim run $$update leads set score = score + 2$$ score 'select 1' keep all",
        &[],
    )
    .unwrap();
    s.close(&db);
    // its own tenant's row changed, the other's not
    let scores = sql::run(&db, "sim_0", "select id, score from leads order by id", &[]).unwrap();
    assert_eq!(scores[0].rows, vec![vec![Value::Int(1), Value::Int(12)], vec![Value::Int(2), Value::Int(20)]]);
    // Parse and Describe ask the same of a tenant agent's statement
    let _as = chronos::acting_as("run7");
    let st = &sql::parse("update leads set score = $1 where id = $2 returning score").unwrap()[0];
    assert_eq!(st.params(&db, "base7", &[]).unwrap(), [sql::Type::Int, sql::Type::Int]);
    assert!(st.columns(&db, "base7", &[sql::Type::Int, sql::Type::Int]).unwrap().is_some());
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

/// Expires world `w` now and lets the clock pass it.
fn expire_now(db: &Db, w: &str) {
    db.expire(w, Some(now_ms())).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
}

/// A world set's members live by the lifecycle as any world does: an expired member is no best to merge (MERGE BEST
/// refuses, 55000, and keeps every member), members can't outlive their base, and none fork from a base whose time is
/// up.
#[test]
fn set_members_keep_their_expiry() {
    let db = Db::new();
    ranked_set(&db);
    run(&db, MAIN, "rank world set s by ($$select sum(bal) from accounts$$) with checks");
    expire_now(&db, "s_3"); // the best
    let e = sql::run(&db, MAIN, "merge best of set s into plan", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    for w in ["s_0", "s_1", "s_2", "s_3"] {
        assert!(db.world(w).is_ok(), "{w} kept: nothing merged, nothing discarded");
    }
    assert_eq!(run(&db, "plan", "select bal from accounts where id = 1").rows[0][0], Value::Float(100.0));

    run(&db, MAIN, "alter world plan set ttl '1 hour'");
    run(&db, MAIN, "fork world set s count 1 run $$update accounts set bal = 1 where id = 3$$");
    let plan = db.world("plan").unwrap().expires;
    assert!(plan.is_some());
    assert_eq!(db.world("s_4").unwrap().expires, plan, "a member can't outlive its base");
    expire_now(&db, "plan");
    let e = sql::run(&db, MAIN, "fork world set s count 1 run $$select 1$$", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
}

/// Members' writes count toward their base's budget, as any fork's do: past it, a member's script is refused
/// (53400) and the member kept with that error.
#[test]
fn set_members_charge_their_base_budget() {
    let db = Db::new();
    run(&db, MAIN, "create table accounts (id bigint primary key, bal double precision)");
    run(&db, MAIN, "insert into accounts select g, 100 from generate_series(1, 10) g");
    run(&db, MAIN, "create world plan with (budget_rows = 3)");
    run(&db, MAIN, "create world set s from world plan");
    let spec = chronos::ForkSet {
        set: "s".into(),
        count: 2,
        script: "update accounts set bal = 0 where id <= 2".into(),
        seed: 1,
        threads: 1,
        agent: None,
    };
    let made = db.fork_world_set(&spec).unwrap();
    assert_eq!(made.len(), 2);
    let errors: Vec<&String> = made.iter().filter_map(|m| m.error.as_ref()).collect();
    assert_eq!(errors.len(), 1, "the first member's 2 rows fit the base's 3, the second's don't: {made:?}");
    assert!(errors[0].contains("budget_rows"), "{}", errors[0]);
}

/// An objective without SUBJECT TO CHECKS ranks by its query alone: a merge check its members all fail doesn't mark
/// them failed, and KEEP keeps the best of them.
#[test]
fn objective_without_checks_ignores_merge_checks() {
    let db = Db::new();
    setup(&db, 20);
    db.create_check("no_overdraft", "select id from accounts where bal < 0", &["accounts".into()], None, false)
        .unwrap();
    run(&db, MAIN, "create objective lowest minimize $$select sum(bal) from accounts$$");
    // every member overdraws account 1: the check fails for each
    let script = "$$update accounts set bal = -10 - $1 where id = 1$$";
    let o = run(&db, MAIN, &format!("optimize lowest from world plan run {script} count 4 keep 2 seed 1"));
    assert_eq!(o.tag, "OPTIMIZE 4");
    assert!(o.rows.iter().all(|r| r[4] == Value::Bool(false)), "none failed: {:?}", o.rows);
    let kept = o.rows.iter().filter(|r| r[6] == Value::Bool(true)).count();
    assert_eq!(kept, 2, "{:?}", o.rows);
}

/// A set's members get their base's tenant identity, as any fork does: a base of no tenant gives global members, a
/// tenant's base gives that tenant's (the security release's rule T1; PR #362 review).
#[test]
fn set_members_take_the_bases_tenant_identity() {
    let db = Db::new();
    setup(&db, 10);
    db.fork_with(MAIN, "t_plan", serde_json::json!({"tenant": "id = 3"})).unwrap();
    run(&db, MAIN, "create world set g from world plan; create world set t from world t_plan");
    run(&db, MAIN, &format!("fork world set g count 2 run {SCRIPT} seed 1"));
    run(&db, MAIN, &format!("fork world set t count 2 run {SCRIPT} seed 1"));
    let worlds = run(&db, MAIN, "show worlds");
    let cols = worlds.columns.as_ref().unwrap();
    let at = |name: &str| cols.iter().position(|(n, _)| n == name).unwrap();
    let (name, status) = (at("name"), at("tenant_status"));
    for r in &worlds.rows {
        let w = r[name].to_text();
        let want = match () {
            _ if w.starts_with("g_") => "global",
            _ if w.starts_with("t_") => "tenant",
            _ => continue,
        };
        assert_eq!(r[status].to_text(), want, "{w}");
    }
    assert_eq!(db.world("t_1").or_else(|_| db.world("t_0")).unwrap().scope.unwrap()["tenant"]["value"], 3);
}
