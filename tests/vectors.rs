//! Vector search once a table is big enough for an HNSW graph: checked against brute force on
//! main and inside a fork that moved and deleted rows, with and without filters.

use chronos::{Db, Query};
use serde_json::{Value, json};
use std::collections::HashSet;

const N: u64 = 60_000; // over the size that gets a graph, with CHRONOS_SCAN_WORDS (see `graphs`)
const DIM: usize = 16;

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
    fn unit(&mut self) -> Vec<f32> {
        let v: Vec<f32> = (0..DIM).map(|_| self.below(2001) as f32 / 1000.0 - 1.0).collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter().map(|x| x / n).collect()
    }
}

fn load(db: &Db, r: &mut Rng) {
    for chunk in (0..N).collect::<Vec<_>>().chunks(10_000) {
        let rows: Vec<(String, String)> = chunk
            .iter()
            .map(|i| (format!("docs/{i:06}"), json!({ "team": i % 50, "emb": r.unit() }).to_string()))
            .collect();
        db.batch("main", &rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
    }
}

/// The true top `k` keys by dot product among a branch's rows passing `team`.
fn truth(db: &Db, b: &str, q: &[f32], team: Option<u64>, k: usize) -> Vec<String> {
    let mut s: Vec<(f32, String)> = db
        .prefix(b, "docs/")
        .unwrap()
        .into_iter()
        .filter_map(|(key, v)| {
            let v: Value = serde_json::from_str(&v).ok()?;
            if team.is_some_and(|t| v["team"] != json!(t)) {
                return None;
            }
            let e: Vec<f32> = v["emb"].as_array()?.iter().map(|x| x.as_f64().unwrap() as f32).collect();
            Some((e.iter().zip(q).map(|(a, b)| a * b).sum(), key))
        })
        .collect();
    s.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    s.into_iter().take(k).map(|(_, k)| k).collect()
}

fn find(db: &Db, b: &str, q: &[f32], team: Option<u64>, k: usize) -> Vec<String> {
    let filters = team.map(|t| vec![("team".to_string(), json!(t))]).unwrap_or_default();
    let query =
        Query { table: "docs".into(), filters, vector: Some(("emb".into(), q.to_vec())), k, ..Default::default() };
    db.find(b, &query).unwrap().1.into_iter().map(|h| h.key).collect()
}

fn wait_for_graphs(db: &Db) {
    let start = std::time::Instant::now();
    while !db.search_graphs_ready() {
        assert!(start.elapsed().as_secs() < 300, "the graph never finished building");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Recall of `find` against brute force over `queries`, as a fraction.
fn recall(db: &Db, b: &str, r: &mut Rng, team: Option<u64>, queries: usize) -> f64 {
    let mut hit = 0;
    for _ in 0..queries {
        let q = r.unit();
        let got: HashSet<String> = find(db, b, &q, team, 10).into_iter().collect();
        hit += truth(db, b, &q, team, 10).iter().filter(|k| got.contains(*k)).count();
    }
    hit as f64 / (queries * 10) as f64
}

/// Makes tables this small get a graph (at the default, 16-dimension vectors are scanned exactly
/// up to millions of rows).
fn graphs() {
    // SAFETY: before any search in this test binary reads it
    unsafe { std::env::set_var("CHRONOS_SCAN_WORDS", "10000") };
}

#[test]
fn graph_search_matches_brute_force_on_main_and_in_a_fork() {
    graphs();
    let db = Db::new();
    let mut r = Rng(42);
    load(&db, &mut r);
    let _ = find(&db, "main", &r.unit(), None, 10); // the first search starts the build
    wait_for_graphs(&db);

    let unfiltered = recall(&db, "main", &mut r, None, 40);
    assert!(unfiltered >= 0.95, "unfiltered recall {unfiltered}");
    // a narrow filter (2% of rows) is scanned exactly
    assert_eq!(recall(&db, "main", &mut r, Some(7), 20), 1.0);

    // a fork moves some rows right next to the query and deletes others
    db.fork("main", "f").unwrap();
    let q = r.unit();
    let before = find(&db, "f", &q, None, 10);
    let moved: Vec<(String, String)> =
        (0..5).map(|i| (format!("docs/{:06}", 100 + i), json!({ "team": 1, "emb": q }).to_string())).collect();
    let mut writes: Vec<(&str, Option<&str>)> = moved.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect();
    writes.extend(before[..3].iter().map(|k| (k.as_str(), None))); // deleted: must not come back
    db.batch("f", &writes).unwrap();
    let got = find(&db, "f", &q, None, 10);
    for (k, _) in &moved {
        assert!(got.contains(k), "a moved row is found at its new place: {got:?}");
    }
    for k in &before[..3] {
        assert!(!got.contains(k), "a deleted row never comes back: {k}");
    }
    let in_fork = recall(&db, "f", &mut r, None, 20);
    assert!(in_fork >= 0.95, "recall inside the fork {in_fork}");
    assert!(recall(&db, "main", &mut r, None, 10) >= 0.95, "main is unaffected");
}
