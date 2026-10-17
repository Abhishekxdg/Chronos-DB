//! Vector search: roaring filter bitmaps, then either an HNSW graph walk (large candidate sets,
//! once the graph is built) or a binary-quantized scan with exact f32 rescoring (small ones).
//! ponytail: flat (multi-core) scan over one immutable index. Phase 3 adds HNSW, explicit SIMD,
//! multi-core scans, text search, and a base + delta index per fork.

use crate::hnsw::{Hnsw, Space};
use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Candidate sets are scanned exactly (1-bit codes, the best rescored at full precision) while
/// their codes come to at most `scan_limit()` 64-bit words: about 350,000 vectors of 1,536 dimensions,
/// 1.4 million of 384. On 76,000 real OpenAI embeddings that found 99.8% of the true top 10 in
/// about 0.4 ms, where the graph peaked at 95.5% (examples/walk.rs). Past it, and for filters that
/// keep at least half the rows, the graph (built only for fields that big, see `wants_graph`);
/// narrower filters scan, since a graph walk would pass mostly rows they reject.
/// ponytail: one fixed budget for every machine; per-core throughput would move it.
/// `CHRONOS_SCAN_WORDS` sets it: higher scans more (recall) before the graph takes over (speed).
fn scan_limit() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| std::env::var("CHRONOS_SCAN_WORDS").ok().and_then(|n| n.parse().ok()).unwrap_or(1 << 23))
}
/// Candidate sets whose vectors come to at most this many numbers are compared whole: exact, and
/// cheap enough (a filter keeping 1% of 500,000 vectors of 384 dimensions: 97.4% recall with 1-bit
/// codes in 0.44 ms, 100% this way in 0.50 ms on the free cores).
const EXACT_FLOATS: u64 = 1 << 22;
/// Candidate sets this small (in code words) are always scanned: a fraction of a millisecond.
const SMALL_SCAN: u64 = 1 << 20;
/// A graph that finds at least this share of the true top 10 near its own vectors is used as
/// soon as the scan would cost more than `SMALL_SCAN`; a weaker one only past the scan limit.
/// Its own vectors flatter it (a graph measuring 97% got 92.8% on new queries, and one measuring
/// 98% served 96% recall), so the bar is high; clustered synthetic data measures 99.5%.
const GOOD: f32 = 0.99;
/// Up to this many dimensions a scan compares the vectors themselves, however many: sign bits
/// would be too few to rank by.
const EXACT_DIM: usize = 32;
/// The graph's beam per search before it's tuned (widened for bigger k).
const EF_MIN: usize = 400;
const EF_MAX: usize = 2048;
/// Beams `calibrate` tries, narrowest first; it keeps the first that finds `TUNED_RECALL` of the
/// true top 10, else the widest. A narrower beam is faster: at 500,000 x 384, 256 gave 99.8% in
/// 0.61 ms where 400 gave 99.9% in 0.81 ms. It stops at 400: on 1M real OpenAI embeddings (1,536
/// dimensions) no beam reached the bar, and 1,024 took 2.33 ms for 99.36% of the true top 10
/// where 400 took 1.00 ms for 98.52% (examples/walk.rs); `SET hnsw.ef_search` buys the rest.
const EF_LADDER: [usize; 8] = [64, 96, 128, 160, 200, 256, 320, 400];
const TUNED_RECALL: f32 = 0.995;
/// How far `calibrate` moves its probe queries off the stored vectors (as a share of their length),
/// so they're as hard as new queries: a stored vector itself is found too easily. Fitted at 500,000
/// x 384: 0.35 and 0.5 both chose beam 256 (99.8% on held-out queries); 0.2 chose 160 (99.1%).
const PROBE_NOISE: f32 = 0.35;
/// From this many dimensions the graph is walked on 1-bit codes (a step is a few popcounts, about
/// 5x cheaper, with the same recall at 1,536 dimensions); below it the codes are too coarse to
/// steer by, and it walks on the vectors themselves.
const CODE_WALK_DIM: usize = 512;

/// Whether HNSW graphs are used (`CHRONOS_VECTOR=flat` turns them off, to compare).
fn graphs_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("CHRONOS_VECTOR").map_or(true, |v| v != "flat"))
}

/// Why a search outside `_checked` can't fail: only a statement stops (see `sql::par_run`), and
/// statements search through `search_in_ef_checked` and `exact_by_checked`.
const UNSTOPPED: &str = "only a statement's search can be stopped, and statements use the _checked searches";

/// Candidate sets at least this big are scanned on every core.
const PARALLEL_MIN: u64 = 50_000;

/// What a search ranks by. The index ranks by dot product; for vectors of unit length that's the
/// same order as by cosine or Euclidean (L2) distance, and for others `exact_by` compares them all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Metric {
    Dot,
    Cosine,
    L2,
}

/// `v` as a copy for distance `m` keeps it (see `VecIndex::for_metric`): of unit length for cosine, whose order
/// inner product then gives; as it is for the others (L2's copy has a graph by Euclidean distance itself).
pub fn for_index(m: Metric, v: &[f32]) -> Vec<f32> {
    match m {
        Metric::Cosine => {
            let n = dot(v, v).sqrt();
            if n > 0.0 && n.is_finite() { v.iter().map(|x| x / n).collect() } else { v.to_vec() }
        }
        _ => v.to_vec(),
    }
}

/// A query `q` for a copy made by `for_index`, rewritten as it rewrites vectors.
pub fn for_query(m: Metric, q: &[f32]) -> Vec<f32> {
    for_index(m, q)
}

/// A store ranked by Euclidean distance, for the graph of an L2 copy (see `VecIndex::for_metric`): between two
/// stored vectors `-|a - b|²`, from their inner product and the squared lengths kept; from a query `2 q·v - |v|²`,
/// which ranks by distance to `q` leaving out `|q|²`, the same for every `v`. A graph walks well by a true distance,
/// where one by inner product on rewritten vectors found only 90% of the true neighbours.
struct L2Space<'a> {
    store: &'a Store,
    norms: &'a [f32],
}

impl Space for L2Space<'_> {
    fn len(&self) -> usize {
        self.store.len()
    }
    fn between(&self, a: u32, b: u32) -> f32 {
        2.0 * self.store.between(a, b) - self.norms[a as usize] - self.norms[b as usize]
    }
    fn to(&self, q: &[f32], id: u32) -> f32 {
        2.0 * self.store.to(q, id) - self.norms[id as usize]
    }
    fn fetch(&self, id: u32) {
        self.store.fetch(id)
    }
}

/// How near `v` is to `q` by `m`, higher nearer: the dot product, cosine similarity (with `q` of
/// unit length), or the negated squared distance. A zero vector has no angle: it comes last.
pub fn similarity(m: Metric, q: &[f32], v: &[f32]) -> f32 {
    // in f32 unless a step overflows it (numbers above about 1.8e19, whose squares don't fit):
    // then in f64, so a huge vector gets its true score rather than 0 or NaN
    let wide = || -> f32 {
        let d: f64 = q.iter().zip(v).map(|(a, b)| *a as f64 * *b as f64).sum();
        match m {
            Metric::Dot => d as f32,
            Metric::Cosine => {
                let n = v.iter().map(|x| *x as f64 * *x as f64).sum::<f64>().sqrt();
                if n > 0.0 { (d / n) as f32 } else { f32::NEG_INFINITY }
            }
            Metric::L2 => -q.iter().zip(v).map(|(a, b)| (*a as f64 - *b as f64).powi(2)).sum::<f64>() as f32,
        }
    };
    let s = match m {
        Metric::Dot => dot(q, v),
        Metric::Cosine => {
            let n = dot(v, v).sqrt();
            if !n.is_finite() {
                return wide();
            }
            if n > 0.0 {
                dot(q, v) / n
            } else if v.iter().all(|&x| x == 0.0) {
                f32::NEG_INFINITY
            } else {
                return wide(); // a length that underflowed: tiny numbers, not a zero vector
            }
        }
        Metric::L2 => -q.iter().zip(v).map(|(a, b)| (a - b) * (a - b)).sum::<f32>(),
    };
    if s.is_finite() || s == f32::NEG_INFINITY && m == Metric::Cosine { s } else { wide() }
}

pub struct VecIndex {
    dim: usize,
    /// every vector added has unit length, but for all-zero ones (placeholders, in `zeros`): so
    /// ranking the rest by dot product ranks them by any metric
    unit: bool,
    zeros: RoaringBitmap,
    words: usize,
    bits: Vec<u64>, // one sign bit per dimension, `words` u64s per vector: 32x smaller than f32
    store: Store,   // the numbers, for rescoring the candidates and comparing small sets whole
    tags: HashMap<String, RoaringBitmap>, // "field=value" -> ids
    all: RoaringBitmap,
    graph: OnceLock<Hnsw>,
    /// the graph's recall@10 against the scan, measured on its own vectors once it's built (see
    /// `calibrate`); searches use the graph only past the scan limit unless it's at least GOOD
    graph_recall: OnceLock<f32>,
    building: AtomicBool,
    /// the graph's search beam; 0: the default (see `search_in`)
    ef: AtomicUsize,
    /// the default beam `calibrate` chose for this graph
    tuned_ef: OnceLock<usize>,
    /// a copy for L2 (see `for_metric`): its graph is by Euclidean distance, with each vector's squared length here
    l2: Option<Vec<f32>>,
}

impl VecIndex {
    pub fn new(dim: usize) -> Self {
        let words = dim.div_ceil(64);
        VecIndex {
            dim,
            unit: true,
            zeros: RoaringBitmap::new(),
            words,
            bits: vec![],
            store: Store::new(dim, 0),
            tags: HashMap::new(),
            all: RoaringBitmap::new(),
            graph: OnceLock::new(),
            graph_recall: OnceLock::new(),
            building: AtomicBool::new(false),
            ef: AtomicUsize::new(0),
            tuned_ef: OnceLock::new(),
            l2: None,
        }
    }

    /// Whether this index keeps 8-bit numbers (see `Store`): its scores are close, not exact.
    pub fn approximate(&self) -> bool {
        matches!(self.store, Store::Fine(_))
    }

    /// An index that keeps 8-bit numbers however few vectors it has (see `Store`): for tests and
    /// benchmarks of what big indexes do.
    pub fn eight_bit(dim: usize) -> Self {
        VecIndex { store: Store::Fine(Fine::new(dim)), ..VecIndex::new(dim) }
    }

    /// The index `add` and `pad_to` would build from `rows`: each vector at its id, zero vectors
    /// between. The rows' numbers are moved into place rather than copied.
    pub fn from_rows(rows: VecRows) -> Self {
        let VecRows { dim, words, ids, mut store, mut bits, zeros, unit } = rows;
        let mut ix = VecIndex::new(dim);
        let n = ids.last().map_or(0, |&l| l as usize + 1);
        store.resize(n);
        bits.resize(n * words, 0);
        // last first: ids are ascending and at least their position, so each vector moves to or
        // past its own place, over room already moved out of; the place it leaves is a gap unless
        // an earlier vector moves there later
        for (i, &id) in ids.iter().enumerate().rev() {
            let (from, to) = (i, id as usize);
            if from != to {
                store.move_to(from, to);
                bits.copy_within(from * words..(from + 1) * words, to * words);
                bits[from * words..(from + 1) * words].fill(0);
            }
        }
        (ix.store, ix.bits, ix.zeros, ix.unit) = (store, bits, zeros, unit);
        ix.all = ids.iter().copied().collect();
        ix
    }

    /// Adds zero vectors (matching nothing) until there are `n`.
    pub fn pad_to(&mut self, n: usize) {
        if self.len() < n {
            let missing = n - self.len();
            self.bits.resize(self.bits.len() + missing * self.words, 0);
            self.store.resize(n);
        }
    }

    pub fn len(&self) -> usize {
        self.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.len() == 0
    }

    /// Adds a vector (normalize it for cosine) with equality-filterable tags; returns its id.
    pub fn add(&mut self, v: &[f32], tags: &[(&str, &str)]) -> u32 {
        assert_eq!(v.len(), self.dim, "vector has {} dims, index has {}", v.len(), self.dim);
        let id = self.len() as u32;
        let n = dot(v, v);
        // by its numbers: the squared length of a vector of tiny ones underflows to zero
        if v.iter().all(|&x| x == 0.0) {
            self.zeros.insert(id); // scores the same as any other zero vector: see `zeros`
        } else {
            self.unit &= (n - 1.0).abs() <= 1e-3;
        }
        self.bits.extend(signs(v, self.words));
        self.store.push(v, None);
        for (f, x) in tags {
            self.tags.entry(format!("{f}={x}")).or_default().insert(id);
        }
        self.all.insert(id);
        id
    }

    /// Top-k by dot product among rows matching every filter. Ranks by Hamming distance on sign
    /// bits, then rescores the best `k * oversample` candidates at full precision.
    pub fn search(&self, q: &[f32], filter: &[(&str, &str)], k: usize, oversample: usize) -> Vec<(u32, f32)> {
        self.search_in(q, &self.filter(filter), k, oversample)
    }

    /// Builds the HNSW graph over the vectors added so far, on every core, unless it's built or
    /// being built. Searches use it once it's there. Returns whether this call built it.
    pub fn build_graph(&self) -> bool {
        if self.graph.get().is_some() || self.building.swap(true, Ordering::AcqRel) {
            return false;
        }
        let g = match &self.l2 {
            Some(norms) => Hnsw::build(&L2Space { store: &self.store, norms }),
            None => Hnsw::build(&self.store),
        };
        let _ = self.graph.set(g);
        let _ = self.graph_recall.set(self.calibrate());
        true
    }

    /// Chooses the graph's default beam and returns its recall@10 there: for probe queries near
    /// 128 of the index's own vectors (moved off them by `PROBE_NOISE`, each leaving its vector
    /// out), the narrowest beam of `EF_LADDER` that finds `TUNED_RECALL` of their exact top 10;
    /// else the widest. How well a graph does depends on the data: 99.9% on clustered synthetic
    /// vectors, 93-95% on real OpenAI embeddings of near-duplicate entities (examples/walk.rs), so
    /// the beam is measured, not fixed.
    fn calibrate(&self) -> f32 {
        // probes near real vectors only: padding (rows without one) is zeros, an all-zero probe
        let n = self.all.len() as usize;
        let probes = 128.min(n);
        let allow = (n < self.len()).then_some(&self.all);
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut gauss = move || {
            // Box-Muller from two xorshift draws
            let mut u = || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                ((rng >> 11) as f64 + 0.5) / (1u64 << 53) as f64
            };
            let (a, b) = (u(), u());
            ((-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()) as f32
        };
        // (probe, its vector's id, its true top 10 without that vector)
        let cases: Vec<(Vec<f32>, u32, Vec<u32>)> = (0..probes)
            .map(|i| {
                let id = self.all.select((i * n / probes) as u32).expect("fewer probes than vectors");
                let v = self.store.get(id);
                let len = dot(&v, &v).sqrt();
                let sd = PROBE_NOISE * len / (self.dim as f32).sqrt();
                let q: Vec<f32> = v.iter().map(|x| x + sd * gauss()).collect();
                // exact, not `scan`: the 1-bit scan loses recall as the index grows (83.8% of the
                // true top 10 at 2M x 384), and a graph graded against it can't score above it
                // ponytail: one full pass over the vectors per probe (128 passes); batch the probes
                // into one pass if that shows up in build time at tens of millions of vectors
                let want = self
                    .exact_in(&q, &self.all, 11, if self.l2.is_some() { Metric::L2 } else { Metric::Dot })
                    .expect(UNSTOPPED)
                    .into_iter()
                    .map(|(x, _)| x)
                    .filter(|&x| x != id)
                    .take(10)
                    .collect();
                (q, id, want)
            })
            .collect();
        let recall_at = |ef: usize| {
            let (mut hit, mut total) = (0, 0);
            for (q, id, want) in &cases {
                let got: Vec<u32> = self
                    .walk(q, 11, ef, allow, self.dim >= CODE_WALK_DIM)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(x, _)| x)
                    .filter(|x| x != id)
                    .take(10)
                    .collect();
                hit += want.iter().filter(|x| got.contains(x)).count();
                total += want.len();
            }
            if total == 0 { 0.0 } else { hit as f32 / total as f32 }
        };
        let mut chosen = (*EF_LADDER.last().unwrap(), 0.0);
        for ef in EF_LADDER {
            let r = recall_at(ef);
            chosen = (ef, r);
            if r >= TUNED_RECALL {
                break;
            }
        }
        let _ = self.tuned_ef.set(chosen.0);
        chosen.1
    }

    /// The default beam `calibrate` chose, once the graph is built.
    pub fn tuned_ef(&self) -> Option<usize> {
        self.tuned_ef.get().copied()
    }

    /// The graph's measured recall (see `calibrate`), once it's built.
    pub fn graph_recall(&self) -> Option<f32> {
        self.graph_recall.get().copied()
    }

    /// Sets the graph's search beam (0: the default). Wider finds more of the true nearest,
    /// slower. For tuning and benchmarks.
    pub fn set_ef(&self, ef: usize) {
        self.ef.store(ef, Ordering::Relaxed);
    }

    /// Whether the graph is built and has tested itself (`calibrate`, which runs after the build
    /// on every core): until then the index is still working in the background.
    pub fn has_graph(&self) -> bool {
        self.graph_recall.get().is_some()
    }

    /// `search` over an explicit candidate set of ids: through the graph when it's built and the
    /// set is large, else an exact scan (which also backs up a graph walk that comes up short).
    pub fn search_in(&self, q: &[f32], bm: &RoaringBitmap, k: usize, oversample: usize) -> Vec<(u32, f32)> {
        self.search_in_ef(q, bm, k, oversample, 0)
    }

    /// `search_in` with the graph's beam for this search (0: the index's, see `set_ef`).
    pub fn search_in_ef(
        &self,
        q: &[f32],
        bm: &RoaringBitmap,
        k: usize,
        oversample: usize,
        ef: usize,
    ) -> Vec<(u32, f32)> {
        self.search_in_ef_checked(q, bm, k, oversample, ef).expect(UNSTOPPED)
    }

    /// `search_in_ef` inside a statement: an error when the statement stops (a cancel, its
    /// timeout) while the search runs on the free cores (see `sql::par_run`).
    pub(crate) fn search_in_ef_checked(
        &self,
        q: &[f32],
        bm: &RoaringBitmap,
        k: usize,
        oversample: usize,
        ef: usize,
    ) -> crate::sql::Result<Vec<(u32, f32)>> {
        // never more than there are candidates: a huge k (a LIMIT, a page) sizes buffers below
        let k = k.min(bm.len() as usize);
        if k == 0 {
            return Ok(vec![]);
        }
        let n = self.len() as u64;
        let cost = bm.len() * self.scan_cost();
        let good = self.graph_recall().is_some_and(|r| r >= GOOD);
        if graphs_on() && bm.len() >= n / 2 && (cost > scan_limit() || (cost > SMALL_SCAN && good)) {
            let ef = match (ef, self.ef.load(Ordering::Relaxed)) {
                (0, 0) => (4 * k).max(self.tuned_ef().unwrap_or(EF_MIN)).min(EF_MAX),
                (0, e) | (e, _) => e.min(n as usize).max(k),
            };
            let allow = (bm.len() < n).then_some(bm);
            if let Some(found) = self.walk(q, k, ef, allow, self.dim >= CODE_WALK_DIM)
                && found.len() >= k.min(bm.len() as usize)
            {
                return Ok(found);
            }
        }
        self.scan(q, bm, k, oversample)
    }

    /// The top `k` through the graph with a beam of `ef`: walked on 1-bit codes, then rescored at
    /// full precision (`codes`), or walked on the vectors. None until the graph is built.
    pub fn walk(
        &self,
        q: &[f32],
        k: usize,
        ef: usize,
        allow: Option<&RoaringBitmap>,
        codes: bool,
    ) -> Option<Vec<(u32, f32)>> {
        let g = self.graph.get()?;
        if let Some(norms) = &self.l2 {
            return Some(g.search(&L2Space { store: &self.store, norms }, q, k, ef, allow));
        }
        if !codes {
            return Some(g.search(&self.store, q, k, ef, allow));
        }
        let qb = signs(q, self.words);
        Some(self.rescore(q, g.candidates(&self.bits, self.words, &qb, ef.max(k), allow).into_iter(), k))
    }

    /// A copy of these vectors for distance `m`, to give `m` a graph of its own (see `for_index`): for cosine, of unit
    /// length, which inner product ranks as cosine does; for L2, as they are, with a graph by Euclidean distance (see
    /// `L2Space`). None when this index already ranks by `m` (inner product, or vectors of unit length), or is too
    /// small for a graph.
    pub fn for_metric(&self, m: Metric) -> Option<VecIndex> {
        if m == Metric::Dot || self.unit || !self.wants_graph() {
            return None;
        }
        let mut rows = VecRows::new(self.dim, self.all.len() as usize);
        for id in &self.all {
            rows.push(id, &for_index(m, &self.store.get(id)));
        }
        let mut ix = VecIndex::from_rows(rows);
        ix.pad_to(self.len());
        if m == Metric::L2 {
            ix.l2 = Some((0..ix.len() as u32).map(|id| ix.store.between(id, id)).collect());
        }
        Some(ix)
    }

    /// The top `k` of `bm` by inner product through the graph alone, as `search_in_ef_checked` would walk it: when
    /// the graph is built, measured good (see `calibrate`), and `bm` keeps at least half the vectors. None
    /// otherwise, and when the walk comes up short: the caller then compares exactly. For a graph of
    /// `for_metric`'s, whose 1-bit scan would rank by the wrong thing.
    pub fn graph_only(
        &self,
        q: &[f32],
        bm: &RoaringBitmap,
        k: usize,
        ef: usize,
        codes: bool,
    ) -> Option<Vec<(u32, f32)>> {
        let k = k.min(bm.len() as usize);
        if k == 0 {
            return Some(vec![]);
        }
        let n = self.len() as u64;
        if !graphs_on() || bm.len() < n / 2 || !self.graph_recall().is_some_and(|r| r >= GOOD) {
            return None;
        }
        let ef = match (ef, self.ef.load(Ordering::Relaxed)) {
            (0, 0) => (4 * k).max(self.tuned_ef().unwrap_or(EF_MIN)).min(EF_MAX),
            (0, e) | (e, _) => e.min(n as usize).max(k),
        };
        let allow = (bm.len() < n).then_some(bm);
        self.walk(q, k, ef, allow, codes && self.dim >= CODE_WALK_DIM).filter(|f| f.len() >= k)
    }

    /// Whether this many vectors are worth a graph: more than a quick scan (see `SMALL_SCAN`).
    pub fn wants_graph(&self) -> bool {
        self.len() as u64 * self.scan_cost() > SMALL_SCAN.min(scan_limit())
    }

    /// A vector's share of a scan, in 8-byte words read: its code, or the whole vector when it's
    /// compared whole (scans read memory as fast as it comes, so bytes are the cost).
    fn scan_cost(&self) -> u64 {
        if self.dim <= EXACT_DIM { self.dim.div_ceil(2) as u64 } else { self.words as u64 }
    }

    /// Vectors no level-0 graph link points to (for checking graph quality).
    pub fn graph_orphans(&self) -> Option<usize> {
        Some(self.graph.get()?.orphans())
    }

    /// The 1-bit scan with rescoring, whatever the graph (for comparisons).
    pub fn exact_scan(&self, q: &[f32], bm: &RoaringBitmap, k: usize, oversample: usize) -> Vec<(u32, f32)> {
        self.scan(q, bm, k, oversample).expect(UNSTOPPED)
    }

    fn scan(&self, q: &[f32], bm: &RoaringBitmap, k: usize, oversample: usize) -> crate::sql::Result<Vec<(u32, f32)>> {
        if self.dim <= EXACT_DIM || bm.len() * self.dim as u64 <= EXACT_FLOATS {
            return match &self.store {
                Store::Fine(f) => self.exact_bounded(f, q, bm, k, Metric::Dot),
                Store::Floats { .. } => self.exact_in(q, bm, k, Metric::Dot),
            };
        }
        let qb = signs(q, self.words);
        // fewer dimensions, fewer bits to rank by: rescore more (64 dimensions: 8x, which took
        // 1M synthetic vectors from 91.2% to 99.4% recall@10, 1.2 ms to 2.0 ms)
        let cap = k
            .saturating_mul(oversample)
            .saturating_mul((CODE_WALK_DIM / self.dim).max(1))
            .min(bm.len() as usize)
            .max(1);
        let max_d = self.words * 64;
        // Hamming distances are small integers, so selection is a histogram, not a heap: count
        // how many candidates sit at each distance, find the cutoff that yields `cap`, keep those.
        // every vector a candidate (no filter): walk the codes as one block, not the bitmap
        let every = bm.len() == self.len() as u64;
        let scan = |lo: u64, hi: u64| {
            let mut hist = vec![0usize; max_d + 1];
            let hi = hi.min(self.len() as u64);
            let mut all: Vec<(u32, u32)> =
                Vec::with_capacity(cap.saturating_mul(4).min(hi.saturating_sub(lo) as usize));
            // the cutoff only tightens as vectors are seen: `cut` is where `cap` of those seen
            // so far are at least as near, and anything farther can't make the final cut
            let (mut cut, mut within) = (max_d, 0usize);
            let mut see = |id: u32, d: u32| {
                let d = d as usize;
                if d > cut {
                    return;
                }
                hist[d] += 1;
                within += 1;
                all.push((d as u32, id));
                while cut > 0 && within - hist[cut] >= cap {
                    within -= hist[cut];
                    cut -= 1;
                }
            };
            if every {
                // blocks of distances from the (dispatched) inner loop, then the cutoff
                let mut ds = Vec::with_capacity(4096);
                let mut at = lo as usize;
                while at < hi as usize {
                    let end = (at + 4096).min(hi as usize);
                    ds.clear();
                    code_dists(&self.bits[at * self.words..end * self.words], self.words, &qb, &mut ds);
                    for (j, &d) in ds.iter().enumerate() {
                        see((at + j) as u32, d);
                    }
                    at = end;
                }
            } else {
                for id in bm.range(lo as u32..hi as u32) {
                    see(id, hamming(&self.bits[id as usize * self.words..][..self.words], &qb));
                }
            }
            keep_nearest(all, &hist, cap)
        };
        let mut cands = if bm.len() < PARALLEL_MIN {
            scan(0, u64::MAX)
        } else {
            // parts of the id range, on the cores no other query is using (see `sql::par_run`)
            let parts = bm.len().div_ceil(PARALLEL_MIN / 2);
            let step = (self.len() as u64).div_ceil(parts);
            let found = crate::sql::par_run(parts as usize, |i, _| Ok(scan(i as u64 * step, (i as u64 + 1) * step)));
            found?.into_iter().flatten().collect()
        };
        if cands.len() > cap {
            cands.select_nth_unstable(cap - 1);
            cands.truncate(cap);
        }
        self.rescore_par(q, cands.into_iter().map(|(_, id)| id).collect(), k)
    }

    /// `rescore` in parts on the free cores, when there are enough candidates to be worth it.
    fn rescore_par(&self, q: &[f32], ids: Vec<u32>, k: usize) -> crate::sql::Result<Vec<(u32, f32)>> {
        const PART: usize = 256;
        if ids.len() < 2 * PART {
            return Ok(self.rescore(q, ids.into_iter(), k));
        }
        let parts = ids.len().div_ceil(PART);
        let tops = crate::sql::par_run(parts, |i, _| {
            Ok(self.rescore(q, ids[i * PART..ids.len().min((i + 1) * PART)].iter().copied(), k))
        });
        let mut all: Vec<(u32, f32)> = tops?.into_iter().flatten().collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        all.truncate(k);
        Ok(all)
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Similarity (dot product) of one stored vector to `q` (from its 8-bit numbers in a big
    /// index, see `Store`).
    pub fn score(&self, id: u32, q: &[f32]) -> f32 {
        self.store.to(q, id)
    }

    /// `similarity` of one stored vector to `q`.
    pub fn score_by(&self, id: u32, q: &[f32], m: Metric) -> f32 {
        self.store.similarity(m, q, id)
    }

    /// Whether every vector added has unit length, or none at all (see `Metric`, `zeros`).
    pub fn unit(&self) -> bool {
        self.unit
    }

    /// The all-zero vectors added: by any metric they score alike, apart from the unit ones.
    pub fn zeros(&self) -> &RoaringBitmap {
        &self.zeros
    }

    /// The top `k` of `bm` by `m`, comparing every vector (on the free cores): for metrics the
    /// index can't rank by.
    /// An index of 8-bit numbers can't compare exactly: it returns every vector that could be in
    /// the true top `k` instead (see `exact_bounded`), for ranking from the rows.
    pub fn exact_by(&self, q: &[f32], bm: &RoaringBitmap, k: usize, m: Metric) -> Vec<(u32, f32)> {
        self.exact_by_checked(q, bm, k, m).expect(UNSTOPPED)
    }

    /// `exact_by` inside a statement (see `search_in_ef_checked`).
    pub(crate) fn exact_by_checked(
        &self,
        q: &[f32],
        bm: &RoaringBitmap,
        k: usize,
        m: Metric,
    ) -> crate::sql::Result<Vec<(u32, f32)>> {
        match &self.store {
            Store::Fine(f) => self.exact_bounded(f, q, bm, k, m),
            Store::Floats { .. } => self.exact_in(q, bm, k, m),
        }
    }

    /// Brute-force ground truth: every filtered row at full precision (at the 8-bit numbers for an
    /// index that keeps them: see `Store`; one from `VecIndex::new` always keeps floats).
    pub fn exact(&self, q: &[f32], filter: &[(&str, &str)], k: usize) -> Vec<(u32, f32)> {
        self.rescore(q, self.filter(filter).iter(), k)
    }

    fn filter(&self, filter: &[(&str, &str)]) -> RoaringBitmap {
        let mut sets = vec![];
        for (f, x) in filter {
            match self.tags.get(&format!("{f}={x}")) {
                Some(s) => sets.push(s),
                None => return RoaringBitmap::new(),
            }
        }
        sets.sort_by_key(|s| s.len());
        let Some((first, rest)) = sets.split_first() else { return self.all.clone() };
        rest.iter().fold((*first).clone(), |acc, s| acc & *s)
    }

    /// The top `k` of `bm` by comparing every vector, in parts on the cores no query is using
    /// (reading scattered vectors is what costs here, so more cores help).
    fn exact_in(&self, q: &[f32], bm: &RoaringBitmap, k: usize, m: Metric) -> crate::sql::Result<Vec<(u32, f32)>> {
        let rescore = |ids: roaring::bitmap::Iter| match m {
            Metric::Dot => self.rescore(q, ids, k),
            m => self.top(ids.map(|id| (id, self.score_by(id, q, m))).collect(), k),
        };
        if bm.len() < 4096 {
            return Ok(rescore(bm.iter()));
        }
        let parts = bm.len().div_ceil(2048);
        let step = (self.len() as u64).div_ceil(parts);
        let tops = crate::sql::par_run(parts as usize, |i, _| {
            let (lo, hi) = (i as u64 * step, ((i as u64 + 1) * step).min(u32::MAX as u64));
            Ok(rescore(bm.range(lo as u32..hi as u32)))
        });
        let mut all: Vec<(u32, f32)> = tops?.into_iter().flatten().collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        all.truncate(k);
        Ok(all)
    }

    /// `exact_in` for an index of 8-bit numbers, where it can't be exact: every vector of `bm`
    /// that could be in the true top `k` by `m`, best first by its 8-bit score, so the caller can
    /// rank them from their exact vectors (`index::TableIndex::search`). Each score comes with how
    /// far off it can be (see `Fine::bounded`); a vector is kept if its score plus that could
    /// reach the `k`th best score less its own. Near-duplicates rounded to the same numbers are all
    /// kept; usually few more than `k` are.
    fn exact_bounded(
        &self,
        f: &Fine,
        q: &[f32],
        bm: &RoaringBitmap,
        k: usize,
        m: Metric,
    ) -> crate::sql::Result<Vec<(u32, f32)>> {
        if k == 0 {
            return Ok(vec![]);
        }
        // in f64, and rounded up into f32, so it bounds the true sum from above
        let l1 = (q.iter().map(|x| x.abs() as f64).sum::<f64>() * (1.0 + 1e-6)) as f32;
        let qn2 = dot(q, q);
        let rough = |ids: roaring::bitmap::Iter| -> Vec<(u32, f32, f32)> {
            ids.map(|id| {
                let (s, e) = f.bounded(m, q, id, l1, qn2);
                (id, s, e)
            })
            .collect()
        };
        let all: Vec<(u32, f32, f32)> = if bm.len() < 4096 {
            rough(bm.iter())
        } else {
            let parts = bm.len().div_ceil(2048);
            let step = (self.len() as u64).div_ceil(parts);
            let found = crate::sql::par_run(parts as usize, |i, _| {
                let (lo, hi) = (i as u64 * step, ((i as u64 + 1) * step).min(u32::MAX as u64));
                Ok(rough(bm.range(lo as u32..hi as u32)))
            });
            found?.into_iter().flatten().collect()
        };
        if all.len() <= k {
            let mut all: Vec<(u32, f32)> = all.into_iter().map(|(id, s, _)| (id, s)).collect();
            all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            return Ok(all);
        }
        // the kth best score any vector is sure of; a score or bound that isn't finite (numbers
        // so big their squares overflow f32) is sure of nothing and always kept, for the rows
        let sure = |s: f32, e: f32| s.is_finite() && e.is_finite();
        let mut floor: Vec<f32> =
            all.iter().map(|&(_, s, e)| if sure(s, e) { s - e } else { f32::NEG_INFINITY }).collect();
        floor.select_nth_unstable_by(k - 1, |a, b| b.total_cmp(a));
        let bar = floor[k - 1];
        let mut kept: Vec<(u32, f32)> =
            all.into_iter().filter(|&(_, s, e)| !sure(s, e) || s + e >= bar).map(|(id, s, _)| (id, s)).collect();
        kept.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(kept)
    }

    fn rescore(&self, q: &[f32], ids: impl Iterator<Item = u32>, k: usize) -> Vec<(u32, f32)> {
        self.top(ids.map(|id| (id, self.score(id, q))).collect(), k)
    }

    /// The `k` best scored, best first (ties by id).
    fn top(&self, mut v: Vec<(u32, f32)>, k: usize) -> Vec<(u32, f32)> {
        let best = |a: &(u32, f32), b: &(u32, f32)| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0));
        if v.len() > k {
            if k == 0 {
                return vec![];
            }
            v.select_nth_unstable_by(k - 1, best); // the best k first, in no order
            v.truncate(k);
        }
        v.sort_by(best);
        v
    }
}

/// The `cap` entries with the smallest distance, given a histogram of the distances.
fn keep_nearest(all: Vec<(u32, u32)>, hist: &[usize], cap: usize) -> Vec<(u32, u32)> {
    let (mut cut, mut below) = (0, 0);
    while cut < hist.len() && below + hist[cut] < cap {
        below += hist[cut];
        cut += 1;
    }
    let mut at_cut = cap.saturating_sub(below); // how many at exactly `cut` still fit
    all.into_iter()
        .filter(|&(d, _)| {
            let d = d as usize;
            d < cut
                || (d == cut && at_cut > 0 && {
                    at_cut -= 1;
                    true
                })
        })
        .collect()
}

/// Vectors given a row at a time for `VecIndex::from_rows`: rounded to 8 bits as they come, so
/// their floats are never all held, with their sign bits and ids.
pub struct VecRows {
    dim: usize,
    words: usize,
    ids: Vec<u32>,
    store: Store,
    bits: Vec<u64>,
    zeros: RoaringBitmap,
    unit: bool,
}

impl VecRows {
    /// Rows of vectors of `dim` numbers, with room for `room` of them.
    pub fn new(dim: usize, room: usize) -> Self {
        let words = dim.div_ceil(64);
        let store = Store::new(dim, room);
        VecRows {
            dim,
            words,
            ids: vec![],
            store,
            bits: Vec::with_capacity(room.saturating_mul(words)),
            zeros: RoaringBitmap::new(),
            unit: true,
        }
    }

    /// Adds id `id`'s vector; ids come in increasing order.
    pub fn push(&mut self, id: u32, v: &[f32]) {
        assert_eq!(v.len(), self.dim, "vector has {} dims, rows have {}", v.len(), self.dim);
        let n = dot(v, v);
        // by its numbers: the squared length of a vector of tiny ones underflows to zero
        if v.iter().all(|&x| x == 0.0) {
            self.zeros.insert(id); // see `VecIndex::zeros`
        } else {
            self.unit &= (n - 1.0).abs() <= 1e-3;
        }
        self.bits.extend(signs(v, self.words));
        self.store.push(v, Some(float_limit()));
        self.ids.push(id);
    }

    /// How many vectors were added.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The ids added, in order.
    pub fn ids(&self) -> &[u32] {
        &self.ids
    }
}

/// Numbers a vector column keeps in memory (see `Store`), as floats: at most this many (256 MB).
/// `CHRONOS_VECTOR_FLOATS` sets it; 0 keeps 8-bit numbers for every column.
fn float_limit() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::env::var("CHRONOS_VECTOR_FLOATS").ok().and_then(|n| n.parse().ok()).unwrap_or(1 << 26))
}

/// A vector column's numbers: floats while it holds at most `float_limit()` of them, so small
/// tables are compared exactly as documented, and 8-bit ones (`Fine`) above, 4 times smaller.
/// Only the search index's own build (`VecRows`) rounds, since its searches rank their last
/// candidates from the rows (`index::TableIndex::search`); an index made with `VecIndex::new`
/// keeps its floats however big, so its `exact` stays ground truth. A column that grows past the
/// limit is rounded then, once.
enum Store {
    Floats { dim: usize, v: Vec<f32> },
    Fine(Fine),
}

impl Store {
    /// For about `room` vectors of `dim` numbers.
    fn new(dim: usize, room: usize) -> Self {
        if room.saturating_mul(dim) > float_limit() {
            let mut f = Fine::new(dim);
            f.codes.reserve_exact(room * dim);
            f.most.reserve_exact(room);
            Store::Fine(f)
        } else {
            Store::Floats { dim, v: Vec::with_capacity(room * dim) }
        }
    }

    fn len(&self) -> usize {
        match self {
            Store::Floats { dim, v } => v.len() / dim.max(&1),
            Store::Fine(f) => f.len(),
        }
    }

    /// Adds a vector; floats past `limit` numbers (None: never) are rounded to 8 bits.
    fn push(&mut self, x: &[f32], limit: Option<usize>) {
        match self {
            Store::Floats { dim, v } => {
                v.extend_from_slice(x);
                if limit.is_some_and(|l| v.len() > l) {
                    let mut f = Fine::new(*dim);
                    for x in v.chunks_exact(*dim) {
                        f.push(x);
                    }
                    *self = Store::Fine(f);
                }
            }
            Store::Fine(f) => f.push(x),
        }
    }

    /// Zero vectors until there are `n`.
    fn resize(&mut self, n: usize) {
        match self {
            Store::Floats { dim, v } => v.resize(n * *dim, 0.0),
            Store::Fine(f) => f.resize(n),
        }
    }

    /// Moves vector `from` to `to`, leaving a zero vector (see `VecIndex::from_rows`).
    fn move_to(&mut self, from: usize, to: usize) {
        match self {
            Store::Floats { dim, v } => {
                let d = *dim;
                v.copy_within(from * d..(from + 1) * d, to * d);
                v[from * d..(from + 1) * d].fill(0.0);
            }
            Store::Fine(f) => {
                let d = f.dim;
                f.codes.copy_within(from * d..(from + 1) * d, to * d);
                f.codes[from * d..(from + 1) * d].fill(0);
                f.most[to] = std::mem::take(&mut f.most[from]);
            }
        }
    }

    fn floats(v: &[f32], dim: usize, id: u32) -> &[f32] {
        &v[id as usize * dim..][..dim]
    }

    /// Vector `id` as floats (as rounded, if it is).
    fn get(&self, id: u32) -> Vec<f32> {
        match self {
            Store::Floats { dim, v } => Store::floats(v, *dim, id).to_vec(),
            Store::Fine(f) => f.get(id),
        }
    }

    /// `similarity` of vector `id` to `q`.
    fn similarity(&self, m: Metric, q: &[f32], id: u32) -> f32 {
        match self {
            Store::Floats { dim, v } => similarity(m, q, Store::floats(v, *dim, id)),
            Store::Fine(f) => f.similarity(m, q, id),
        }
    }
}

impl Space for Store {
    fn len(&self) -> usize {
        Store::len(self)
    }
    fn between(&self, a: u32, b: u32) -> f32 {
        match self {
            Store::Floats { dim, v } => dot(Store::floats(v, *dim, a), Store::floats(v, *dim, b)),
            Store::Fine(f) => f.between(a, b),
        }
    }
    fn to(&self, q: &[f32], id: u32) -> f32 {
        match self {
            Store::Floats { dim, v } => dot(q, Store::floats(v, *dim, id)),
            Store::Fine(f) => f.to(q, id),
        }
    }
    fn fetch(&self, id: u32) {
        match self {
            Store::Floats { dim, v } => prefetch(&Store::floats(v, *dim, id)[..(*dim).min(128)]),
            Store::Fine(f) => f.fetch(id),
        }
    }
}

/// Vectors at 8 bits a number, each with its own scale: a number is its code times the vector's
/// largest number (by size, `most`) over 127, so that one is +-127. `most` is kept rather than
/// `most / 127`, which rounds to zero for vectors whose numbers are all below about 9e-44; scores
/// are taken in f64 and rounded once (see `wide`), so neither tiny nor huge numbers lose them. 4 times smaller than floats (at 1M x
/// 1,536, 1.5 GB where floats took 6.1 GB). Only the stored side is rounded, by at most half a
/// step (1/254 of the vector's largest number); queries stay floats, and SQL scores the rows a
/// search returns from the rows themselves.
pub(crate) struct Fine {
    dim: usize,
    codes: Vec<i8>,
    most: Vec<f32>,
}

impl Fine {
    fn new(dim: usize) -> Self {
        Fine { dim, codes: vec![], most: vec![] }
    }

    fn len(&self) -> usize {
        self.most.len()
    }

    fn push(&mut self, v: &[f32]) {
        let most = v.iter().fold(0f32, |m, x| m.max(x.abs()));
        if most == 0.0 {
            self.codes.resize(self.codes.len() + self.dim, 0);
            self.most.push(0.0);
            return;
        }
        // in f64: 127 / most overflows f32 below about 4e-37, and an f32 ratio near a midpoint can
        // pick the neighbouring code, off by more than the half step `bounded` allows
        let m = most as f64;
        self.codes.extend(v.iter().map(|&x| (x as f64 / m * 127.0).round() as i8));
        self.most.push(most);
    }

    /// Zero vectors until there are `n` (or fewer vectors, cut to `n`).
    fn resize(&mut self, n: usize) {
        self.codes.resize(n * self.dim, 0);
        self.most.resize(n, 0.0);
    }

    fn codes(&self, id: u32) -> &[i8] {
        &self.codes[id as usize * self.dim..][..self.dim]
    }

    /// Vector `id` as floats, as rounded.
    fn get(&self, id: u32) -> Vec<f32> {
        let m = self.most[id as usize] as f64;
        self.codes(id).iter().map(|&c| (c as f64 * m / 127.0) as f32).collect()
    }

    /// `similarity` of vector `id` to `q` by `m` from its 8-bit numbers, and how far off that can
    /// be. Each number is off by at most half a step (`most / 254`), so the dot product by at most
    /// `e1 = most / 254 * |q|` summed (`l1`) and the vector's length by at most `e2 = most / 254 *
    /// sqrt(dim)`; `qn2` is `q`'s squared length. A vector too short for its cosine to be bounded
    /// can be off by anything; a zero vector is exact. The slack covers rounding in the sums.
    fn bounded(&self, m: Metric, q: &[f32], id: u32, l1: f32, qn2: f32) -> (f32, f32) {
        let most = self.most[id as usize];
        let d = self.to(q, id);
        // half a step per number, plus what summing `dim` products in f32 can lose (at most `dim`
        // units in the last place of their summed sizes, `most * l1` at most), and a millionth for
        // the rest of the arithmetic
        let gamma = self.dim as f64 * f32::EPSILON as f64;
        let e1 = (most as f64 * l1 as f64 * (1.0 / 254.0 + gamma) * (1.0 + 1e-6)) as f32;
        let e2 = wide(most, (self.dim as f32).sqrt(), 254.0) * (1.0 + 1e-6);
        let slack = 1e-5 * (1.0 + l1 + qn2) + qn2 * (self.dim as f32 * f32::EPSILON);
        match m {
            Metric::Dot => (d, e1 + slack),
            Metric::Cosine => {
                let n = self.norm(id);
                if self.is_zero(id) {
                    (f32::NEG_INFINITY, 0.0)
                } else if n <= e2 {
                    (if n > 0.0 { d / n } else { 0.0 }, f32::INFINITY)
                } else {
                    let c = d / n;
                    (c, (e1 + c.abs() * e2) / (n - e2) + slack)
                }
            }
            Metric::L2 => {
                let n2 = self.norm2(id);
                (-(qn2 - 2.0 * d + n2), 2.0 * e1 + e2 * (2.0 * n2.sqrt() + e2) + slack)
            }
        }
    }

    /// The squared length of vector `id`.
    fn norm2(&self, id: u32) -> f32 {
        let n = self.norm(id);
        n * n
    }

    /// The length of vector `id`: its scale times the codes' length, not the scale squared
    /// first, which underflows for vectors of tiny numbers and would make them look like zero.
    fn norm(&self, id: u32) -> f32 {
        (self.most[id as usize] as f64 * (dot_88(self.codes(id), self.codes(id)) as f64).sqrt() / 127.0) as f32
    }

    /// Whether vector `id` is all zeros (a nonzero one always has a code of +-127).
    fn is_zero(&self, id: u32) -> bool {
        self.codes(id).iter().all(|&c| c == 0)
    }

    /// `similarity` of vector `id` to `q`.
    fn similarity(&self, m: Metric, q: &[f32], id: u32) -> f32 {
        match m {
            Metric::Dot => self.to(q, id),
            Metric::Cosine => {
                let n = self.norm(id);
                match (self.is_zero(id), n > 0.0) {
                    (true, _) => f32::NEG_INFINITY,
                    (false, true) => self.to(q, id) / n,
                    (false, false) => 0.0, // numbers too small for their length to show in f32
                }
            }
            Metric::L2 => -(dot(q, q) - 2.0 * self.to(q, id) + self.norm2(id)),
        }
    }
}

impl Space for Fine {
    fn len(&self) -> usize {
        self.most.len()
    }
    fn between(&self, a: u32, b: u32) -> f32 {
        let (ma, mb) = (self.most[a as usize] as f64, self.most[b as usize] as f64);
        (ma * dot_88(self.codes(a), self.codes(b)) as f64 * mb / (127.0 * 127.0)) as f32
    }
    fn to(&self, q: &[f32], id: u32) -> f32 {
        let c = self.codes(id);
        let d = dot_q8(q, c);
        // a query number near f32::MAX times a code overflows f32 though the dot product may
        // not: only then, again in f64
        let d = if d.is_finite() { d as f64 } else { q.iter().zip(c).map(|(x, y)| *x as f64 * *y as f64).sum() };
        (self.most[id as usize] as f64 * d / 127.0) as f32
    }
    fn fetch(&self, id: u32) {
        prefetch(&self.codes(id)[..self.dim.min(512)]);
    }
}

/// `a * b / d` in f64, rounded to f32 once: `most` times a sum of codes overflows f32 for vectors
/// of numbers near `f32::MAX`, and `most / 127` underflows for ones below about 9e-44, though the
/// result is an f32 either way.
fn wide(a: f32, b: f32, d: f64) -> f32 {
    (a as f64 * b as f64 / d) as f32
}

/// The dot product of floats `q` and 8-bit numbers `c` (before their scale).
fn dot_q8(q: &[f32], c: &[i8]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    if avx2() {
        // SAFETY: the CPU has AVX2 and FMA (checked just above)
        return unsafe { dot_q8_avx2(q, c) };
    }
    dot_q8_any(q, c)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_q8_avx2(q: &[f32], c: &[i8]) -> f32 {
    dot_q8_any(q, c)
}

/// Eight accumulators, as in `dot_any`, so it vectorizes.
#[inline(always)]
fn dot_q8_any(q: &[f32], c: &[i8]) -> f32 {
    let mut acc = [0f32; 8];
    for (x, y) in q.chunks_exact(8).zip(c.chunks_exact(8)) {
        for ((s, x), y) in acc.iter_mut().zip(x).zip(y) {
            *s += x * *y as f32;
        }
    }
    let tail = q.chunks_exact(8).remainder().iter().zip(c.chunks_exact(8).remainder()).map(|(x, y)| x * *y as f32);
    acc.iter().sum::<f32>() + tail.sum::<f32>()
}

/// The dot product of two runs of 8-bit numbers, exactly, however long (see `dot_88_any`).
fn dot_88(a: &[i8], b: &[i8]) -> i64 {
    #[cfg(target_arch = "x86_64")]
    if avx2() {
        // SAFETY: the CPU has AVX2 (checked just above)
        return unsafe { dot_88_avx2(a, b) };
    }
    dot_88_any(a, b)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_88_avx2(a: &[i8], b: &[i8]) -> i64 {
    dot_88_any(a, b)
}

#[inline(always)]
fn dot_88_any(a: &[i8], b: &[i8]) -> i64 {
    // i32 lanes over blocks small enough that none can overflow (4,096 products of at most
    // 127 * 127 per lane), summed in i64: vectors of any length, JSON ones having no limit
    let mut total = 0i64;
    for (a, b) in a.chunks(1 << 16).zip(b.chunks(1 << 16)) {
        let mut acc = [0i32; 16];
        for (x, y) in a.chunks_exact(16).zip(b.chunks_exact(16)) {
            for ((s, x), y) in acc.iter_mut().zip(x).zip(y) {
                *s += *x as i32 * *y as i32;
            }
        }
        let tail = a.chunks_exact(16).remainder().iter().zip(b.chunks_exact(16).remainder());
        total += acc.iter().map(|&s| s as i64).sum::<i64>() + tail.map(|(x, y)| *x as i64 * *y as i64).sum::<i64>();
    }
    total
}

fn signs(v: &[f32], words: usize) -> Vec<u64> {
    let mut out = vec![0u64; words];
    for (w, chunk) in out.iter_mut().zip(v.chunks(64)) {
        // branch-free, so it vectorizes: a bit per positive number
        *w = chunk.iter().enumerate().fold(0u64, |b, (j, &x)| b | (((x > 0.0) as u64) << j));
    }
    out
}

/// Whether this x86-64 CPU has AVX2 and FMA. Release builds assume only x86-64-v2 (POPCNT, SSE4.2,
/// see .cargo/config.toml) so they run on CPUs from 2009 on; the hot loops below also come in an
/// AVX2 build, picked here at run time (twice the floats per instruction). Checked once, cached.
#[cfg(target_arch = "x86_64")]
fn avx2() -> bool {
    std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
}

/// Eight independent accumulators so LLVM vectorizes it (it won't reorder a single float sum).
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    if avx2() {
        // SAFETY: the CPU has AVX2 and FMA (checked just above)
        return unsafe { dot_avx2(a, b) };
    }
    dot_any(a, b)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_avx2(a: &[f32], b: &[f32]) -> f32 {
    dot_any(a, b)
}

/// Asks the CPU to start loading `s` into its caches: a hint, which never faults (none where the
/// target has no stable prefetch instruction).
#[inline(always)]
pub(crate) fn prefetch<T>(s: &[T]) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        let p = s.as_ptr() as *const i8;
        for at in (0..std::mem::size_of_val(s)).step_by(64) {
            // SAFETY: a prefetch only hints; the address is within `s`
            unsafe { _mm_prefetch::<_MM_HINT_T0>(p.add(at)) };
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = s;
}

/// Bits that differ between two codes.
pub(crate) fn hamming(a: &[u64], b: &[u64]) -> u32 {
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

/// `hamming` from `qb` to each of the codes laid end to end in `codes`, into `out`: the scan's
/// inner loop, in an AVX2 build where the CPU has it (popcounts over vector registers).
fn code_dists(codes: &[u64], words: usize, qb: &[u64], out: &mut Vec<u32>) {
    #[cfg(target_arch = "x86_64")]
    if avx2() {
        // SAFETY: the CPU has AVX2 (checked just above)
        return unsafe { code_dists_avx2(codes, words, qb, out) };
    }
    code_dists_any(codes, words, qb, out)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn code_dists_avx2(codes: &[u64], words: usize, qb: &[u64], out: &mut Vec<u32>) {
    code_dists_any(codes, words, qb, out)
}

#[inline(always)]
fn code_dists_any(codes: &[u64], words: usize, qb: &[u64], out: &mut Vec<u32>) {
    out.extend(codes.chunks_exact(words).map(|b| b.iter().zip(qb).map(|(x, y)| (x ^ y).count_ones()).sum::<u32>()));
}

#[inline(always)]
fn dot_any(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    for (x, y) in a.chunks_exact(8).zip(b.chunks_exact(8)) {
        for ((s, x), y) in acc.iter_mut().zip(x).zip(y) {
            *s += x * y;
        }
    }
    let tail = a.chunks_exact(8).remainder().iter().zip(b.chunks_exact(8).remainder()).map(|(x, y)| x * y);
    acc.iter().sum::<f32>() + tail.sum::<f32>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact scores of huge vectors (numbers whose squares overflow f32) are their true scores,
    /// not 0 or NaN, and a vector of tiny numbers isn't taken for a zero vector.
    #[test]
    fn exact_scores_of_huge_and_tiny_vectors() {
        let q = [0.6f32, 0.8];
        let huge = [-3e30f32, 4e30];
        assert!(
            (similarity(Metric::Cosine, &q, &huge) - 0.28).abs() < 1e-6,
            "{}",
            similarity(Metric::Cosine, &q, &huge)
        );
        assert!((similarity(Metric::Dot, &q, &huge) - 1.4e30).abs() < 1e25);
        assert!(
            similarity(Metric::L2, &q, &huge).is_finite() || similarity(Metric::L2, &q, &huge) == f32::NEG_INFINITY
        );
        let tiny = [3e-30f32, 4e-30];
        assert!((similarity(Metric::Cosine, &q, &tiny) - 1.0).abs() < 1e-6);
        assert_eq!(similarity(Metric::Cosine, &q, &[0.0, 0.0]), f32::NEG_INFINITY);
    }

    /// Vectors of tiny numbers keep their length and direction at 8 bits: their cosine isn't
    /// taken for a zero vector's, and their codes don't overflow.
    #[test]
    fn tiny_vectors_stay_nonzero() {
        let mut f = Fine::new(64);
        let tiny: Vec<f32> = (0..64).map(|i| if i % 2 == 0 { 1e-22 } else { -1e-22 }).collect();
        f.push(&tiny);
        f.push(&[1e-38; 64]);
        let q: Vec<f32> = (0..64).map(|i| if i % 2 == 0 { 0.125 } else { -0.125 }).collect();
        assert!((f.similarity(Metric::Cosine, &q, 0) - 1.0).abs() < 1e-3, "{}", f.similarity(Metric::Cosine, &q, 0));
        assert!(f.codes(1).iter().all(|&c| c == 127), "codes of 1e-38s: {:?}", &f.codes(1)[..4]);
        let (c, e) = f.bounded(Metric::Cosine, &q, 0, 8.0, 1.0);
        assert!(c > 0.99 && e.is_finite(), "bounded cosine {c} +- {e}");
        // one whose numbers are all about 5e-44: most / 127 would round to zero, its score doesn't
        f.push(&[5e-44; 64]);
        let ones = vec![1.0f32; 64];
        assert!(f.to(&ones, 2) > 0.0, "the score of a vector of 5e-44s: {}", f.to(&ones, 2));
        // and one of numbers near f32::MAX: most times the codes would overflow f32 first
        let mut big = Fine::new(1);
        big.push(&[f32::MAX]);
        assert_eq!(big.to(&[1.0], 0), f32::MAX);
        // a number near a rounding midpoint takes the nearest code, and every number is within
        // the half step `bounded` assumes of its reconstruction
        let mut mid = Fine::new(2);
        mid.push(&[1e30, -5.5511807e29]);
        let back = mid.get(0);
        assert!(
            (back[1] as f64 - -5.5511807e29f64).abs() <= 1e30 / 254.0 * (1.0 + 1e-6),
            "{} for -5.5511807e29",
            back[1]
        );
        // a vector long enough that its codes' squares pass i32::MAX (133,144 numbers of 127)
        let mut long = Fine::new(200_000);
        long.push(&vec![1.0; 200_000]);
        assert!((long.norm(0) - (200_000f32).sqrt()).abs() < 1.0, "length {}", long.norm(0));
        assert!(long.between(0, 0) > 199_000.0, "dot product {}", long.between(0, 0));
        // and a query number near f32::MAX: times a code of 127 it overflows f32 first
        let mut one = Fine::new(1);
        one.push(&[1.0]);
        assert_eq!(one.to(&[f32::MAX], 0), f32::MAX);
        assert_eq!(big.get(0), [f32::MAX]);
        // two of them: their dot product (6.4e-43) shows in f32, the scales' product doesn't
        f.push(&tiny);
        let between = f.between(0, 3);
        assert!(between > 0.0 && (between / 6.4e-43 - 1.0).abs() < 0.05, "dot product of two tiny vectors: {between}");
    }

    /// Only the search index's own build rounds a column past its limit; an index filled with
    /// `add` keeps every float, so its exact scores stay exact.
    #[test]
    fn only_the_index_build_rounds_past_the_limit() {
        let v = [0.6f32, -0.8, 0.0, 0.0];
        let mut kept = Store::new(4, 0);
        let mut built = Store::new(4, 0);
        for _ in 0..3 {
            kept.push(&v, None);
            built.push(&v, Some(8));
        }
        assert!(matches!(kept, Store::Floats { .. }), "add never rounds");
        assert!(matches!(built, Store::Fine(_)), "past 8 numbers the build rounds");
        assert_eq!(kept.to(&v, 2), 1.0);
        assert!((built.to(&v, 2) - 1.0).abs() < 1e-2);
        assert_eq!(built.len(), 3);
    }
}
