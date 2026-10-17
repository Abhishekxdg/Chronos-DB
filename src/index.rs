//! Search indexes (Phase 3). A `TableIndex` covers the rows of one table as of one state: exact-
//! match bitmaps for scalar fields, a BM25 text index over string fields, and a vector index per
//! numeric-array field. It's built once and shared; a branch that has changed a few rows since
//! searches it with those rows masked out, and scores its changed rows directly (base + delta).
//!
//! One query plan: filters first (bitmap intersection), then text and/or vector ranking over what
//! is left, fused with reciprocal rank fusion when both are asked for.
//! Typos: a query word the index has never seen is matched to words one edit away (a letter
//! wrong, missing, extra, or two swapped), at half weight, so exact spellings still rank first.
//! Vectors: an HNSW graph (built in the background) for searches over most of a large table, an
//! exact binary-quantized scan otherwise. ponytail: equality filters only.

use crate::search::{Metric, VecIndex, VecRows, similarity};
use crate::{Change, Val};
use roaring::RoaringBitmap;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const OVERSAMPLE: usize = 64; // vector candidates per result rescored at full precision
/// Candidates per row wanted that an index of 8-bit numbers proposes, for ranking exactly from the
/// rows (see `TableIndex::search`): the 8-bit ranking alone found 94% of the true top 10 on 1M
/// real embeddings. There, over SQL, 2 gave 98.6% of the true top 10 in 1.34 ms and 4 gave 98.7%
/// in 1.44 ms, against 98.7% in 1.32 ms with floats. `CHRONOS_RERANK` sets it (at least 1).
fn rerank() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("CHRONOS_RERANK").ok().and_then(|n| n.parse().ok()).unwrap_or(2).max(1))
}
/// A search stopped with its statement (a cancel, its timeout, see `sql::par_run`), as the
/// database's error, its SQLSTATE kept.
fn stopped(e: crate::sql::SqlError) -> crate::Error {
    crate::Error::Constraint { code: e.code, message: e.message }
}

/// A row's stored vector by key, for ranking a search's last candidates exactly (see
/// `TableIndex::search`); None if the row or its vector is gone.
pub(crate) type ExactVector<'a> = dyn Fn(&str) -> crate::Result<Option<Arc<[f32]>>> + Sync + 'a;

/// Candidates ranked exactly per part, each part on a free core (a row read each).
const EXACT_PART: usize = 4;
const RRF_K: f32 = 60.0;
const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;
const FUZZY_WEIGHT: f32 = 0.5; // a corrected word counts half
const FUZZY_MIN_LEN: usize = 4; // shorter words are too ambiguous to correct
/// Longer words (hashes, ids, base64) aren't corrected: a word's deletions take its length
/// squared in memory, 64 KB for a 256-character hash, so they'd dwarf the table.
const FUZZY_MAX_LEN: usize = 32;

/// A row's value for a filter's field: a top-level field, or `f->>k`, key `k` of object field
/// `f` as SQL's `->>` gives it (text; nothing for null). SQL's `payload->>'user_id' = $1` is
/// the filter ("payload->>user_id", "...").
fn field(row: &serde_json::Map<String, Value>, f: &str) -> Option<Value> {
    match f.split_once("->>") {
        Some((a, k)) if !row.contains_key(f) => crate::sql::json_text(row.get(a)?.get(k)?).map(Value::String),
        _ => row.get(f).cloned(),
    }
}

/// The filter fields of a row and their values: its scalar fields, and `f->>k` for each key of
/// its object fields.
fn filter_fields(row: &serde_json::Map<String, Value>) -> impl Iterator<Item = (String, Value)> + '_ {
    row.iter().flat_map(|(f, x)| -> Vec<(String, Value)> {
        match x {
            Value::Array(_) => vec![],
            Value::Object(o) => o
                .iter()
                .filter_map(|(k, y)| Some((format!("{f}->>{k}"), Value::String(crate::sql::json_text(y)?))))
                .collect(),
            x => vec![(f.clone(), x.clone())],
        }
    })
}

/// A search over one table. Filters are equality on top-level fields (or `f->>k`, see `field`); `text` is ranked with BM25
/// over string fields; `vector` ranks by dot product with a numeric-array field (normalize your
/// vectors for cosine). Both together are fused; neither lists matches in key order.
#[derive(Clone, Debug, Default)]
pub struct Query {
    pub table: String,
    pub filters: Vec<(String, Value)>,
    pub text: Option<String>,
    pub vector: Option<(String, Vec<f32>)>,
    pub k: usize,
    /// Skip this many results first: page n is `offset = n * k`.
    pub offset: usize,
    /// The HNSW graph's beam for this search (0: the default); wider finds more of the true
    /// nearest, slower. SQL's `SET hnsw.ef_search`.
    pub ef: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub key: String,
    pub score: f32,
    pub row: Val,
}

/// A branch's differences from an index: the docs it changed or deleted (hidden from the index)
/// and its own versions of the rows it changed or added (scored directly). Built once per branch
/// version and reused by every query on it.
#[derive(Default)]
pub struct Delta {
    masked: RoaringBitmap,
    extra: Vec<Extra>,
    eq: HashMap<(String, String), Vec<usize>>, // like TableIndex::eq, over `extra`
}

/// One of a branch's own rows, pre-digested for scoring.
struct Extra {
    key: String,
    row: serde_json::Map<String, Value>,
    tf: HashMap<String, u32>,
    len: u32,
    vectors: HashMap<String, Vec<f32>>,
}

pub struct TableIndex {
    keys: Vec<String>, // doc id -> row key; ids follow key order
    doc: HashMap<String, u32>,
    all: RoaringBitmap,
    eq: HashMap<(String, String), RoaringBitmap>, // (field, value as JSON) -> docs
    terms: HashMap<String, Vec<(u32, u32)>>,      // term -> (doc, term frequency)
    lens: Vec<u32>,                               // tokens per doc
    avg_len: f32,
    vectors: HashMap<String, (VecIndex, RoaringBitmap)>, // field -> vectors, docs that have one
    fuzzy: HashMap<String, Vec<String>>,                 // a word and its one-letter deletions -> words (SymSpell)
}

/// A word and every way of deleting one letter from it.
fn variants(w: &str) -> Vec<String> {
    let chars: Vec<char> = w.chars().collect();
    let mut v: Vec<String> = (0..chars.len()).map(|i| chars[..i].iter().chain(&chars[i + 1..]).collect()).collect();
    v.push(w.to_string());
    v
}

/// Edit distance with adjacent swaps counting as one edit (optimal string alignment).
fn edits(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, x) in d[0].iter_mut().enumerate() {
        *x = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

pub fn tokens(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_lowercase)
}

/// A non-empty array of numbers that fit an f32 (a vector with an infinity or NaN in it can't be
/// ranked, so it isn't one).
fn as_vector(v: &Value) -> Option<Vec<f32>> {
    let a = v.as_array().filter(|a| !a.is_empty())?;
    a.iter().map(|x| x.as_f64().map(|f| f as f32).filter(|f| f.is_finite())).collect()
}

/// String fields' tokens with their counts, and the total.
fn doc_terms(o: &serde_json::Map<String, Value>) -> (HashMap<String, u32>, u32) {
    let mut tf = HashMap::new();
    let mut len = 0;
    for s in o.values().filter_map(Value::as_str) {
        for t in tokens(s) {
            *tf.entry(t).or_insert(0) += 1;
            len += 1;
        }
    }
    (tf, len)
}

impl TableIndex {
    /// Indexes rows given in key order. Rows that aren't JSON objects are kept but match nothing
    /// except an unfiltered listing.
    /// `vectors` are fields given as floats rather than in the row's JSON (SQL's vector columns).
    pub fn build(rows: impl IntoIterator<Item = (String, Val, Vec<(String, Arc<[f32]>)>)>) -> TableIndex {
        let mut ix = TableIndex {
            keys: vec![],
            doc: HashMap::new(),
            all: RoaringBitmap::new(),
            eq: HashMap::new(),
            terms: HashMap::new(),
            lens: vec![],
            avg_len: 0.0,
            vectors: HashMap::new(),
            fuzzy: HashMap::new(),
        };
        // each field's vectors, indexed once all are seen: its dimension is the most common length,
        // so a stray short array (or the first row's) doesn't decide it
        // rows may be read as they're made (see `sql::search_rows`), so their vectors are dropped
        // as they're copied in; how many there are sizes each field's numbers (see `FieldVectors`)
        let rows = rows.into_iter();
        let n = rows.size_hint().1.unwrap_or(rows.size_hint().0);
        let mut found: HashMap<String, FieldVectors> = HashMap::new();
        for (key, v, given) in rows {
            let id = ix.keys.len() as u32;
            ix.doc.insert(key.clone(), id);
            ix.keys.push(key);
            ix.all.insert(id);
            let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&v) else {
                ix.lens.push(0);
                continue;
            };
            for (f, x) in &o {
                if let Some(vec) = as_vector(x) {
                    found.entry(f.clone()).or_default().push(id, &vec, n);
                }
            }
            for (f, vec) in given {
                found.entry(f).or_default().push(id, &vec, n);
            }
            for (f, x) in filter_fields(&o) {
                ix.eq.entry((f, x.to_string())).or_default().insert(id);
            }
            let (tf, len) = doc_terms(&o);
            for (t, n) in tf {
                ix.terms.entry(t).or_default().push((id, n));
            }
            ix.lens.push(len);
        }
        for (f, FieldVectors(mut by_len)) in found {
            let Some(dim) = by_len.iter().map(|(&len, r)| (r.len(), std::cmp::Reverse(len))).max().map(|(_, r)| r.0)
            else {
                continue;
            };
            let rows = by_len.remove(&dim).expect("the most common length");
            drop(by_len);
            // rows without this field (or with another length): zero vectors, never candidates
            let present = rows.ids().iter().copied().collect();
            ix.vectors.insert(f, (VecIndex::from_rows(rows), present));
        }
        ix.avg_len = ix.lens.iter().sum::<u32>() as f32 / ix.lens.len().max(1) as f32;
        for t in ix.terms.keys().filter(|t| (FUZZY_MIN_LEN..=FUZZY_MAX_LEN).contains(&t.chars().count())) {
            for v in variants(t) {
                ix.fuzzy.entry(v).or_default().push(t.clone());
            }
        }
        ix
    }

    /// Indexed words one edit away from `w`.
    fn near(&self, w: &str) -> Vec<String> {
        let mut out: Vec<String> = variants(w).iter().filter_map(|v| self.fuzzy.get(v)).flatten().cloned().collect();
        out.sort();
        out.dedup();
        out.retain(|t| t != w && edits(w, t) <= 1);
        out
    }

    /// Query words with their weights (idf): each word as typed, plus, for a word the index has
    /// never seen, its one-typo neighbours at reduced weight.
    fn query_terms(&self, text: &str) -> Vec<(String, f32)> {
        let mut out = vec![];
        for t in tokens(text) {
            if !self.terms.contains_key(&t) && (FUZZY_MIN_LEN..=FUZZY_MAX_LEN + 1).contains(&t.chars().count()) {
                out.extend(self.near(&t).into_iter().map(|c| {
                    let w = self.idf(&c) * FUZZY_WEIGHT;
                    (c, w)
                }));
            }
            let idf = self.idf(&t);
            out.push((t, idf));
        }
        out
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// The dimension of field `f`'s vectors, if it has any.
    pub fn dim(&self, f: &str) -> Option<usize> {
        self.vectors.get(f).map(|(vi, _)| vi.dim())
    }

    /// Starts building the HNSW graph of each vector field big enough to need one, in the
    /// background: searches scan until it's ready, so no query waits for a build. Cheap to call
    /// again (fields already built or building are skipped). One build runs at a time.
    // ponytail: one build at a time across the process, skipped (and retried on the next search)
    // while another runs; a queue if many large tables are searched at once.
    pub fn start_graphs(self: &Arc<Self>) {
        static BUSY: AtomicBool = AtomicBool::new(false);
        if !self.vectors.values().any(|(vi, _)| vi.wants_graph() && !vi.has_graph()) {
            return;
        }
        if BUSY.swap(true, Ordering::AcqRel) {
            return;
        }
        let ix = self.clone();
        std::thread::spawn(move || {
            for (vi, _) in ix.vectors.values().filter(|(vi, _)| vi.wants_graph()) {
                vi.build_graph();
            }
            BUSY.store(false, Ordering::Release);
        });
    }

    /// Whether every vector field that gets a graph has it (for tests and benchmarks).
    pub fn graphs_ready(&self) -> bool {
        self.vectors.values().all(|(vi, _)| !vi.wants_graph() || vi.has_graph())
    }

    pub fn delta(&self, changes: Vec<Change>) -> Delta {
        let mut d = Delta::default();
        for c in changes {
            if let Some(id) = self.doc.get(&c.key) {
                d.masked.insert(*id);
            }
            if let Some(Ok(Value::Object(row))) = c.after.map(|v| serde_json::from_str::<Value>(&v)) {
                let (tf, len) = doc_terms(&row);
                let vectors = row.iter().filter_map(|(f, x)| Some((f.clone(), as_vector(x)?))).collect();
                for (f, x) in filter_fields(&row) {
                    d.eq.entry((f, x.to_string())).or_default().push(d.extra.len());
                }
                d.extra.push(Extra { key: c.key, row, tf, len, vectors });
            }
        }
        d
    }

    fn idf(&self, term: &str) -> f32 {
        let n = self.keys.len() as f32;
        let df = self.terms.get(term).map_or(0, Vec::len) as f32;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    fn bm25(&self, idf: f32, tf: u32, len: u32) -> f32 {
        let tf = tf as f32;
        idf * tf * (BM25_K1 + 1.0) / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * len as f32 / self.avg_len.max(1.0)))
    }

    /// Runs `q` for a branch with delta `d` from this index. Returns the number of matches and
    /// the top `q.k` as (key, score). A match passes the filters and, for a text-only search,
    /// has at least one of the words (with a vector, every row passing the filters has a score).
    /// Vectors rank by `m` (and are scored by `similarity`).
    /// `exact` gives a row's stored vector by key: where the field's index keeps 8-bit numbers
    /// (big tables, see `search::Store`), it proposes `rerank()` times the rows wanted and they're
    /// ranked from their exact vectors, since 8-bit numbers can't tell near-duplicates apart.
    pub fn search(
        &self,
        q: &Query,
        d: &Delta,
        m: Metric,
        exact: &ExactVector,
    ) -> crate::Result<(usize, Vec<(String, f32)>)> {
        let mut cand = &self.all - &d.masked;
        for (f, v) in &q.filters {
            match self.eq.get(&(f.clone(), v.to_string())) {
                Some(bm) => cand &= bm,
                None => cand.clear(),
            }
        }
        let extra: Vec<&Extra> = match q.filters.first() {
            // one filter's short list, then the rest checked row by row
            Some((f, v)) => {
                d.eq.get(&(f.clone(), v.to_string())).map_or(&[][..], |l| &l[..]).iter().map(|&i| &d.extra[i]).collect()
            }
            None => d.extra.iter().collect(),
        };
        let extra: Vec<&Extra> =
            extra.into_iter().filter(|e| q.filters.iter().all(|(f, v)| field(&e.row, f).as_ref() == Some(v))).collect();
        let mut total = cand.len() as usize + extra.len();
        // with text and a vector both ranking, each list runs deeper so the fusion has room
        let want = q.k.max(1).saturating_mul(if q.text.is_some() && q.vector.is_some() { 4 } else { 1 });

        let mut lists: Vec<Vec<(String, f32)>> = vec![];
        if let Some((field, qv)) = &q.vector {
            let mut l: Vec<(String, f32)> = match self.vectors.get(field).filter(|(vi, _)| vi.dim() == qv.len()) {
                Some((vi, present)) => {
                    let c = &cand & present;
                    let rough = vi.approximate();
                    let want = if rough { want.saturating_mul(rerank()) } else { want };
                    // the index ranks by dot product: by any metric for unit vectors (their
                    // scores then given by `m`), else every vector is compared by `m`. Zero
                    // vectors (placeholders) score alike by any metric, so they join as a
                    // group: `want` of them, placed by `m` below
                    let zeros = vi.zeros();
                    let found = match m == Metric::Dot || vi.unit() {
                        true if m == Metric::Dot || zeros.is_disjoint(&c) => {
                            vi.search_in_ef_checked(qv, &c, want, OVERSAMPLE, q.ef).map_err(stopped)?
                        }
                        true => {
                            let mut f =
                                vi.search_in_ef_checked(qv, &(&c - zeros), want, OVERSAMPLE, q.ef).map_err(stopped)?;
                            f.extend((&c & zeros).iter().take(want).map(|id| (id, 0.0)));
                            f
                        }
                        false => vi.exact_by_checked(qv, &c, want, m).map_err(stopped)?,
                    };
                    // a row that can't be read fails the search rather than keep its rough score
                    let scored = |part: &[(u32, f32)]| -> Vec<crate::Result<(String, f32)>> {
                        part.iter()
                            .map(|&(id, s)| {
                                let key = &self.keys[id as usize];
                                // zero vectors keep the place they were given above
                                let score = match rough && !zeros.contains(id) {
                                    true => exact(key)?.filter(|v| v.len() == qv.len()).map(|v| similarity(m, qv, &v)),
                                    false => None,
                                };
                                let score =
                                    score.unwrap_or_else(|| if m == Metric::Dot { s } else { vi.score_by(id, qv, m) });
                                Ok((key.clone(), score))
                            })
                            .collect()
                    };
                    let l: crate::Result<Vec<(String, f32)>> = match rough && found.len() > EXACT_PART {
                        // each exact score reads a row: in parts on the free cores
                        true => {
                            let parts: Vec<&[(u32, f32)]> = found.chunks(EXACT_PART).collect();
                            // the statement stopped (a cancel, its timeout): that error, not a panic
                            let done =
                                crate::sql::par_run(parts.len(), |i, _| Ok(scored(parts[i]))).map_err(stopped)?;
                            done.into_iter().flatten().collect()
                        }
                        false => scored(&found).into_iter().collect(),
                    };
                    l?
                }
                None => vec![],
            };
            for e in &extra {
                if let Some(v) = e.vectors.get(field).filter(|v| v.len() == qv.len()) {
                    l.push((e.key.clone(), similarity(m, qv, v)));
                }
            }
            lists.push(top(l, want));
        }
        if let Some(text) = &q.text {
            let terms = self.query_terms(text);
            // sum per doc by sorting (doc, score) pairs: no hashing, and keys only for the winners
            let mut scored: Vec<(u32, f32)> = vec![];
            for (t, idf) in &terms {
                for &(d, tf) in self.terms.get(t).into_iter().flatten() {
                    if cand.contains(d) {
                        scored.push((d, self.bm25(*idf, tf, self.lens[d as usize])));
                    }
                }
            }
            scored.sort_unstable_by_key(|&(d, _)| d);
            scored.dedup_by(|b, a| {
                let same = a.0 == b.0;
                if same {
                    a.1 += b.1;
                }
                same
            });
            let indexed = scored.len();
            if scored.len() > want {
                scored.select_nth_unstable_by(want - 1, |a, b| b.1.total_cmp(&a.1));
                scored.truncate(want);
            }
            let mut l: Vec<(String, f32)> =
                scored.into_iter().map(|(d, s)| (self.keys[d as usize].clone(), s)).collect();
            let before = l.len();
            for e in &extra {
                let s: f32 = terms.iter().filter_map(|(t, idf)| e.tf.get(t).map(|&n| self.bm25(*idf, n, e.len))).sum();
                if s > 0.0 {
                    l.push((e.key.clone(), s));
                }
            }
            if q.vector.is_none() {
                total = indexed + l.len() - before;
            }
            lists.push(top(l, want));
        }

        let hits = match lists.len() {
            0 => {
                // no ranking asked for: matches in key order
                let mut keys: Vec<String> = cand.iter().take(q.k).map(|d| self.keys[d as usize].clone()).collect();
                keys.extend(extra.iter().map(|e| e.key.clone()));
                keys.sort();
                keys.truncate(q.k);
                keys.into_iter().map(|k| (k, 0.0)).collect()
            }
            1 => lists.pop().unwrap().into_iter().take(q.k).collect(),
            _ => {
                let mut fused: HashMap<String, f32> = HashMap::new();
                for l in lists {
                    for (rank, (k, _)) in l.into_iter().enumerate() {
                        *fused.entry(k).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
                    }
                }
                top(fused.into_iter().collect(), q.k)
            }
        };
        Ok((total, hits))
    }
}

/// Highest scores first (ties by key), at most `n`.
fn top(mut l: Vec<(String, f32)>, n: usize) -> Vec<(String, f32)> {
    l.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    l.truncate(n);
    l
}

/// One field's vectors while indexing, by length. Each is rounded to 8 bits as its row is read
/// and the row's own copy dropped, so a build holds the floats of none of them: holding every
/// row's vector and then copying them all into the index peaked at 14.4 GB for 1M x 1,536
/// (6.1 GB of numbers), and the index keeps 1.5 GB of them now.
#[derive(Default)]
struct FieldVectors(HashMap<usize, VecRows>);

impl FieldVectors {
    /// Adds row `id`'s vector, of `rows` in the table. The field's first length gets room for a
    /// vector per row (the index pads to that size anyway, see `VecIndex::from_rows`), so its
    /// numbers are never moved as they grow: memory the system hands out as it's written, so a
    /// field most rows lack costs only what it holds. Other lengths grow as they go.
    fn push(&mut self, id: u32, v: &[f32], rows: usize) {
        let room = if self.0.is_empty() { rows } else { 0 };
        self.0.entry(v.len()).or_insert_with(|| VecRows::new(v.len(), room)).push(id, v);
    }
}
