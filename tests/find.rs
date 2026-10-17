//! `find`: checked against brute force over the same rows, on main and inside forks that changed,
//! deleted and added rows (which search a shared index plus their own changes).

use chronos::{Db, Query, Resolve};
use serde_json::{Value, json};

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
    fn unit(&mut self) -> Vec<f32> {
        let v: Vec<f32> = (0..16).map(|_| self.below(2001) as f32 / 1000.0 - 1.0).collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter().map(|x| x / n).collect()
    }
}

const COLORS: [&str; 5] = ["red", "green", "blue", "black", "white"];

fn row(r: &mut Rng, i: u64) -> Value {
    let color = COLORS[r.below(5) as usize];
    let size = ["s", "m", "l"][r.below(3) as usize];
    let about =
        format!("{} {}", ["fast", "slow", "cheap"][r.below(3) as usize], ["car", "bike", "boat"][r.below(3) as usize]);
    json!({ "color": color, "size": size, "n": i % 7, "about": about, "embedding": r.unit() })
}

fn load(db: &Db, n: u64, r: &mut Rng) {
    let rows: Vec<(String, String)> = (0..n).map(|i| (format!("items/{i:05}"), row(r, i).to_string())).collect();
    db.batch("main", &rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
}

fn rows(db: &Db, b: &str) -> Vec<(String, Value)> {
    db.prefix(b, "items/").unwrap().into_iter().map(|(k, v)| (k, serde_json::from_str(&v).unwrap())).collect()
}

fn filtered(db: &Db, b: &str, filters: &[(String, Value)]) -> Vec<String> {
    rows(db, b).into_iter().filter(|(_, v)| filters.iter().all(|(f, x)| v.get(f) == Some(x))).map(|(k, _)| k).collect()
}

fn filter_query(r: &mut Rng) -> Vec<(String, Value)> {
    let mut f = vec![];
    if r.below(2) == 0 {
        let c = COLORS[r.below(5) as usize];
        f.push(("color".into(), json!(c)));
    }
    if r.below(2) == 0 {
        let s = ["s", "m", "l"][r.below(3) as usize];
        f.push(("size".into(), json!(s)));
    }
    if r.below(3) == 0 {
        f.push(("n".into(), json!(r.below(7))));
    }
    f
}

/// Filters give exactly the brute-force rows, in key order, with the right count.
fn check_filters(db: &Db, b: &str, r: &mut Rng) {
    for _ in 0..40 {
        let filters = filter_query(r);
        let want = filtered(db, b, &filters);
        let (total, hits) = db
            .find(b, &Query { table: "items".into(), filters: filters.clone(), k: 100_000, ..Default::default() })
            .unwrap();
        assert_eq!(total, want.len(), "{b} {filters:?}");
        assert_eq!(hits.into_iter().map(|h| h.key).collect::<Vec<_>>(), want, "{b} {filters:?}");
    }
}

fn edit_fork(db: &Db, b: &str, r: &mut Rng) {
    db.fork("main", b).unwrap();
    for _ in 0..40 {
        let k = format!("items/{:05}", r.below(2000));
        match r.below(3) {
            0 => db.del(b, &k).unwrap(),
            _ => db.put(b, &k, &row(r, 99).to_string()).unwrap(),
        };
    }
    for i in 0..30 {
        db.put(b, &format!("items/new{i:03}"), &row(r, i).to_string()).unwrap();
    }
}

#[test]
fn filters_on_main_and_forks_match_brute_force() {
    let db = Db::new();
    let mut r = Rng(11);
    load(&db, 2000, &mut r);
    check_filters(&db, "main", &mut r); // builds and caches main's index
    edit_fork(&db, "agent", &mut r);
    check_filters(&db, "agent", &mut r); // main's index + the fork's changes
    check_filters(&db, "main", &mut r);
    db.merge("agent", Resolve::Ours).unwrap();
    check_filters(&db, "main", &mut r);
}

#[test]
fn on_disk_across_checkpoints() {
    let dir = std::env::temp_dir().join(format!("chronos-find-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let mut r = Rng(12);
    load(&db, 2000, &mut r);
    check_filters(&db, "main", &mut r);
    db.checkpoint().unwrap(); // rows move into trees; the cached index stays valid
    edit_fork(&db, "agent", &mut r);
    check_filters(&db, "agent", &mut r);
    db.checkpoint().unwrap();
    check_filters(&db, "agent", &mut r);
    check_filters(&db, "main", &mut r);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn text_ranks_rows_with_more_query_words_first() {
    let db = Db::new();
    for (id, about) in [(1, "red apple pie"), (2, "red car"), (3, "green apple"), (4, "blue boat"), (5, "apple")] {
        db.put("main", &format!("items/{id}"), &json!({ "about": about, "shop": "a" }).to_string()).unwrap();
    }
    let q = |text: &str| Query { table: "items".into(), text: Some(text.into()), k: 10, ..Default::default() };
    let (_, hits) = db.find("main", &q("Red APPLE")).unwrap();
    assert_eq!(hits[0].key, "items/1", "both words beat one: {hits:?}");
    assert_eq!(hits.len(), 4, "rows with neither word don't match: {hits:?}");
    assert!(hits.iter().all(|h| h.key != "items/4"));

    db.fork("main", "f").unwrap();
    db.put("f", "items/4", &json!({ "about": "red apple tart apple", "shop": "a" }).to_string()).unwrap();
    db.del("f", "items/1").unwrap();
    let (_, hits) = db.find("f", &q("red apple")).unwrap();
    assert_eq!(hits[0].key, "items/4", "the fork's edited row: {hits:?}");
    assert!(hits.iter().all(|h| h.key != "items/1"), "deleted in the fork");
    assert_eq!(db.find("main", &q("red apple")).unwrap().1[0].key, "items/1", "main is unaffected");
}

#[test]
fn vectors_with_filters_match_exact_top_k() {
    let db = Db::new();
    let mut r = Rng(13);
    load(&db, 3000, &mut r);
    let mut recall = 0.0;
    for _ in 0..30 {
        let qv = r.unit();
        let c = COLORS[r.below(5) as usize];
        let filters = vec![("color".to_string(), json!(c))];
        let mut exact: Vec<(String, f32)> = rows(&db, "main")
            .into_iter()
            .filter(|(_, v)| v["color"] == filters[0].1)
            .map(|(k, v)| {
                let e: Vec<f32> =
                    v["embedding"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap() as f32).collect();
                (k, e.iter().zip(&qv).map(|(a, b)| a * b).sum())
            })
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1));
        let want: Vec<String> = exact.into_iter().take(10).map(|(k, _)| k).collect();
        let q = Query {
            table: "items".into(),
            filters,
            vector: Some(("embedding".into(), qv)),
            k: 10,
            ..Default::default()
        };
        let got: Vec<String> = db.find("main", &q).unwrap().1.into_iter().map(|h| h.key).collect();
        recall += got.iter().filter(|k| want.contains(k)).count() as f64 / 10.0;
    }
    assert!(recall / 30.0 >= 0.95, "recall@10 {}", recall / 30.0);

    // a fork that writes a row equal to the query finds it first; main doesn't have it
    let qv = r.unit();
    db.fork("main", "f").unwrap();
    db.put("f", "items/zzz", &json!({ "color": "red", "embedding": qv.clone() }).to_string()).unwrap();
    let q = Query { table: "items".into(), vector: Some(("embedding".into(), qv)), k: 3, ..Default::default() };
    assert_eq!(db.find("f", &q).unwrap().1[0].key, "items/zzz");
    assert!(db.find("main", &q).unwrap().1.iter().all(|h| h.key != "items/zzz"));
}

#[test]
fn text_and_vector_fuse() {
    let db = Db::new();
    let up = [1.0f32, 0.0];
    let down = [0.0f32, 1.0];
    for (id, about, v) in [(1, "fast car", up), (2, "fast car", down), (3, "slow boat", up), (4, "slow boat", down)] {
        db.put("main", &format!("items/{id}"), &json!({ "about": about, "e": v }).to_string()).unwrap();
    }
    let q = Query {
        table: "items".into(),
        text: Some("fast car".into()),
        vector: Some(("e".into(), up.to_vec())),
        k: 4,
        ..Default::default()
    };
    let (_, hits) = db.find("main", &q).unwrap();
    assert_eq!(hits[0].key, "items/1", "best on both lists: {hits:?}");
    assert_eq!(hits.last().unwrap().key, "items/4", "worst on both: {hits:?}");
}

#[test]
fn repeated_queries_never_return_stale_answers() {
    let db = Db::new();
    db.put("main", "items/1", &json!({ "color": "red" }).to_string()).unwrap();
    let q = Query { table: "items".into(), filters: vec![("color".into(), json!("red"))], k: 10, ..Default::default() };
    assert_eq!(db.find("main", &q).unwrap().0, 1);
    assert_eq!(db.find("main", &q).unwrap().0, 1); // served from the cache
    db.put("main", "items/2", &json!({ "color": "red" }).to_string()).unwrap();
    assert_eq!(db.find("main", &q).unwrap().0, 2, "a write invalidates the cached answer");
    db.fork("main", "f").unwrap();
    db.del("f", "items/1").unwrap();
    assert_eq!(db.find("f", &q).unwrap().0, 1, "branches don't share answers");
    assert_eq!(db.find("main", &q).unwrap().0, 2);
}

#[test]
fn typos_find_the_intended_word_but_exact_spelling_wins() {
    let db = Db::new();
    for (id, name) in [(1, "John Smith"), (2, "Joan Smyth"), (3, "Jane Doe"), (4, "Bengaluru office")] {
        db.put("main", &format!("people/{id}"), &json!({ "name": name }).to_string()).unwrap();
    }
    let top = |text: &str| {
        let q = Query { table: "people".into(), text: Some(text.into()), k: 5, ..Default::default() };
        db.find("main", &q).unwrap().1.into_iter().map(|h| h.key).collect::<Vec<_>>()
    };
    assert_eq!(top("Jhon")[0], "people/1", "swapped letters");
    assert_eq!(top("Smtih")[0], "people/1");
    assert_eq!(top("Bangaluru")[0], "people/4", "a wrong letter in a place name");
    assert_eq!(top("joan")[0], "people/2", "a real word is never 'corrected' to a neighbour");
    assert!(top("Jxxn").is_empty(), "two edits away is not a match");
    assert!(top("doe")[0] == "people/3" && top("dxe").is_empty(), "short words aren't corrected");
}
