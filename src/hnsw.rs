//! HNSW (hierarchical navigable small world) graph over a set of vectors, for approximate
//! nearest-neighbour search by dot product (normalize vectors for cosine).
//!
//! Each vector is a node with a random level (most are level 0). Every level links each node to
//! a few near neighbours; higher levels are sparse and act as express lanes. A search walks down
//! from the top, greedily, then explores level 0 with a beam of `ef` candidates.
//!
//! Built once over an immutable set of vectors, on every core (a lock per node's links while
//! building), then frozen into flat arrays so searches take no locks. Filtered search walks
//! through any node but only returns allowed ones, so a filter that keeps a fair share of the
//! rows still finds its neighbours; callers send narrow filters to an exact scan instead.

#[cfg(test)]
use crate::search::dot;
use crate::search::prefetch;
use roaring::RoaringBitmap;
use std::cell::RefCell;
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

pub(crate) const M: usize = 16; // links per node on upper levels
const M0: usize = 2 * M; // links per node on level 0
const EF_BUILD: usize = 100; // beam width while building
const NONE: u32 = u32::MAX;

/// A distance: lower is nearer (negated dot product), ordered totally.
#[derive(Clone, Copy, PartialEq)]
struct D(f32);
impl Eq for D {}
impl PartialOrd for D {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for D {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0)
    }
}

pub(crate) struct Hnsw {
    n: usize,
    /// level-0 links, M0 per node, NONE-padded
    links0: Vec<u32>,
    /// upper-level links of the nodes above level 0: node -> links per level (index 0 = level 1)
    upper: HashMap<u32, Vec<Vec<u32>>>,
    entry: u32,
    top: usize,
}

/// A deterministic level for node `id` (geometric, mean 1/ln(M) above 0).
fn level_of(id: u32) -> usize {
    let mut x = (id as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    let u = ((x >> 11) as f64 + 1.0) / (1u64 << 53) as f64; // in (0, 1]
    ((-u.ln()) / (M as f64).ln()).floor().min(16.0) as usize
}

thread_local! {
    /// Which nodes a search has seen: a generation tag per node, so clearing is one increment.
    static SEEN: RefCell<(Vec<u32>, u32)> = const { RefCell::new((Vec::new(), 0)) };
}

/// Runs `f` with a fresh visited set for `n` nodes.
fn with_seen<T>(n: usize, f: impl FnOnce(&mut dyn FnMut(u32) -> bool) -> T) -> T {
    SEEN.with(|s| {
        let mut s = s.borrow_mut();
        let (tags, cur) = &mut *s;
        if tags.len() < n {
            tags.resize(n, 0);
        }
        *cur = cur.wrapping_add(1);
        if *cur == 0 {
            tags.iter_mut().for_each(|t| *t = 0);
            *cur = 1;
        }
        let g = *cur;
        let mut first = |id: u32| {
            let t = &mut tags[id as usize];
            let new = *t != g;
            *t = g;
            new
        };
        f(&mut first)
    })
}

/// The links of one node at one level, for the search loop, into a reused buffer.
trait Links {
    fn links(&self, id: u32, level: usize, out: &mut Vec<u32>);
}

/// Beam search at one level from `eps`: the `ef` nearest nodes found (nearest first) among those
/// `allow` lets through; every node is walked through regardless. `fetch` starts loading what
/// `dist` will read for a node: a node's new neighbours are all fetched before any is measured,
/// so their cache misses overlap (at 1M x 1,536 measuring was 45% of a search, mostly waiting).
#[allow(clippy::too_many_arguments)]
fn search_level(
    links: &dyn Links,
    dist: &dyn Fn(u32) -> f32,
    fetch: &dyn Fn(u32),
    n: usize,
    eps: &[(D, u32)],
    ef: usize,
    level: usize,
    allow: &dyn Fn(u32) -> bool,
) -> Vec<(D, u32)> {
    with_seen(n, |seen| {
        let mut cand: BinaryHeap<Reverse<(D, u32)>> = BinaryHeap::new(); // nearest on top
        let mut best: BinaryHeap<(D, u32)> = BinaryHeap::new(); // farthest kept on top
        for &(d, id) in eps {
            seen(id);
            cand.push(Reverse((d, id)));
            if allow(id) {
                best.push((d, id));
            }
        }
        let mut nbs = Vec::with_capacity(M0);
        while let Some(Reverse((d, id))) = cand.pop() {
            if best.len() >= ef && d > best.peek().unwrap().0 {
                break;
            }
            nbs.clear();
            links.links(id, level, &mut nbs);
            nbs.retain(|&nb| seen(nb));
            for &nb in &nbs {
                fetch(nb);
            }
            for &nb in &nbs {
                let dn = D(dist(nb));
                if best.len() < ef || dn < best.peek().unwrap().0 {
                    cand.push(Reverse((dn, nb)));
                    if allow(nb) {
                        best.push((dn, nb));
                        if best.len() > ef {
                            best.pop();
                        }
                    }
                }
            }
        }
        let mut out = best.into_vec();
        out.sort();
        out
    })
}

/// HNSW's neighbour choice: nearest first, skipping a candidate that is nearer to an already
/// chosen neighbour than to the node, so links spread out in every direction.
fn pick(cands: &[(D, u32)], m: usize, between: &dyn Fn(u32, u32) -> f32) -> Vec<u32> {
    let mut out: Vec<(D, u32)> = vec![];
    for &(d, c) in cands {
        if out.len() == m {
            break;
        }
        if out.iter().all(|&(_, o)| D(between(c, o)) > d) {
            out.push((d, c));
        }
    }
    // fill up with the nearest skipped ones, which keeps the graph well connected
    for &(d, c) in cands {
        if out.len() == m {
            break;
        }
        if !out.iter().any(|&(_, o)| o == c) {
            out.push((d, c));
        }
    }
    out.into_iter().map(|(_, c)| c).collect()
}

/// Links every level-0 node nothing links to from its nearest neighbour that has room (else in
/// place of that neighbour's farthest link), so every vector can be found. Pruning for spread
/// (see `pick`) leaves some nodes of dense clusters (near-duplicates) with no way in, and those
/// are often exactly the true neighbours of queries landing in the cluster.
fn repair(links0: &mut [u32], n: usize, entry: u32, dist: &dyn Fn(u32, u32) -> f32) {
    let mut linked = vec![false; n];
    for &x in links0.iter().filter(|&&x| x != NONE) {
        linked[x as usize] = true;
    }
    let orphans: Vec<u32> = (0..n as u32).filter(|&x| !linked[x as usize] && x != entry).collect();
    for x in orphans {
        let mine: Vec<u32> = links0[x as usize * M0..][..M0].iter().copied().take_while(|&y| y != NONE).collect();
        let Some(&nearest) = mine.first() else { continue };
        let slot = mine.iter().find_map(|&y| {
            let ls = &links0[y as usize * M0..][..M0];
            ls.iter().position(|&z| z == NONE).map(|p| y as usize * M0 + p)
        });
        let slot = slot.unwrap_or_else(|| {
            // the neighbour's farthest link gives way (never the only link into another node: rare)
            let ls = &links0[nearest as usize * M0..][..M0];
            let far = (0..M0).max_by(|&a, &b| dist(nearest, ls[a]).total_cmp(&dist(nearest, ls[b]))).unwrap();
            nearest as usize * M0 + far
        });
        links0[slot] = x;
    }
}

/// Links while building: a lock per node and level.
struct Building {
    links: Vec<Vec<Mutex<Vec<u32>>>>, // node -> level -> links
}

impl Links for Building {
    fn links(&self, id: u32, level: usize, out: &mut Vec<u32>) {
        if let Some(l) = self.links[id as usize].get(level) {
            out.extend_from_slice(&l.lock().unwrap());
        }
    }
}

impl Links for Hnsw {
    fn links(&self, id: u32, level: usize, out: &mut Vec<u32>) {
        if level == 0 {
            out.extend(self.links0[id as usize * M0..][..M0].iter().copied().take_while(|&x| x != NONE));
        } else if let Some(l) = self.upper.get(&id).and_then(|l| l.get(level - 1)) {
            out.extend_from_slice(l);
        }
    }
}

/// Vectors a graph is built over and walked in, ranked by dot product: floats, or the index's
/// 8-bit numbers (`search::Fine`), so the graph needn't keep a full-precision copy.
pub(crate) trait Space: Sync {
    fn len(&self) -> usize;
    /// The dot product of stored vectors `a` and `b`.
    fn between(&self, a: u32, b: u32) -> f32;
    /// The dot product of `q` and stored vector `id`.
    fn to(&self, q: &[f32], id: u32) -> f32;
    /// Starts loading what `between` and `to` read of `id` (see `search_level`).
    fn fetch(&self, id: u32);
}

/// Vectors of `dim` floats laid end to end (the index keeps 8-bit ones: see `search::Fine`).
#[cfg(test)]
pub(crate) struct Floats<'a> {
    pub(crate) vecs: &'a [f32],
    pub(crate) dim: usize,
}

#[cfg(test)]
impl Floats<'_> {
    fn at(&self, id: u32) -> &[f32] {
        &self.vecs[id as usize * self.dim..][..self.dim]
    }
}

#[cfg(test)]
impl Space for Floats<'_> {
    fn len(&self) -> usize {
        self.vecs.len() / self.dim.max(1)
    }
    fn between(&self, a: u32, b: u32) -> f32 {
        dot(self.at(a), self.at(b))
    }
    fn to(&self, q: &[f32], id: u32) -> f32 {
        dot(q, self.at(id))
    }
    fn fetch(&self, id: u32) {
        prefetch(&self.at(id)[..self.dim.min(128)]);
    }
}

impl Hnsw {
    /// Builds the graph over the vectors of `sp`, on every core.
    pub(crate) fn build(sp: &impl Space) -> Hnsw {
        Hnsw::build_on(sp, std::thread::available_parallelism().map_or(4, |n| n.get()))
    }

    /// `build` on `threads` threads. Nodes join the graph in the order the threads take them, so
    /// with more than one the graph (and how well it finds neighbours) varies a little run to run;
    /// with one it's the same every time.
    pub(crate) fn build_on(sp: &impl Space, threads: usize) -> Hnsw {
        let n = sp.len();
        let levels: Vec<usize> = (0..n as u32).map(level_of).collect();
        let b = Building { links: levels.iter().map(|&l| (0..=l).map(|_| Mutex::new(vec![])).collect()).collect() };
        if n == 0 {
            return Hnsw { n, links0: vec![], upper: HashMap::new(), entry: NONE, top: 0 };
        }
        let entry = Mutex::new((0u32, levels[0])); // (entry node, its level)
        let next = AtomicUsize::new(1);
        let insert = |id: u32| {
            let dist = |o: u32| -sp.between(id, o);
            let fetch = |o: u32| sp.fetch(o);
            let lvl = levels[id as usize];
            let (ep, top) = *entry.lock().unwrap();
            let mut eps = vec![(D(dist(ep)), ep)];
            // greedy descent through the levels above this node's own
            for l in (lvl + 1..=top).rev() {
                eps = search_level(&b, &dist, &fetch, n, &eps, 1, l, &|_| true);
            }
            for l in (0..=lvl.min(top)).rev() {
                let found = search_level(&b, &dist, &fetch, n, &eps, EF_BUILD, l, &|_| true);
                let cap = if l == 0 { M0 } else { M };
                let between = |a: u32, c: u32| -sp.between(a, c);
                let mine = pick(&found, M, &between);
                *b.links[id as usize][l].lock().unwrap() = mine.clone();
                for nb in mine {
                    let mut ls = b.links[nb as usize][l].lock().unwrap();
                    ls.push(id);
                    // lists grow to half again their size before a trim back to `cap`: the
                    // careful trim is costly, and this runs it a fraction as often
                    if ls.len() > cap + cap / 2 {
                        let mut c: Vec<(D, u32)> = ls.iter().map(|&x| (D(-sp.between(nb, x)), x)).collect();
                        c.sort();
                        *ls = pick(&c, cap, &between);
                    }
                }
                eps = found;
            }
            let mut e = entry.lock().unwrap();
            if lvl > e.1 {
                *e = (id, lvl);
            }
        };
        let threads = threads.clamp(1, n);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let id = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if id >= n {
                            break;
                        }
                        insert(id as u32);
                    }
                });
            }
        });
        // lists still over their size get their final trim, on every core
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let id = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if id >= n {
                            break;
                        }
                        for (l, m) in b.links[id].iter().enumerate() {
                            let cap = if l == 0 { M0 } else { M };
                            let mut ls = m.lock().unwrap();
                            if ls.len() > cap {
                                let me = id as u32;
                                let mut c: Vec<(D, u32)> = ls.iter().map(|&x| (D(-sp.between(me, x)), x)).collect();
                                c.sort();
                                *ls = pick(&c, cap, &|a, c| -sp.between(a, c));
                            }
                        }
                    }
                });
            }
        });
        // freeze: flat level-0 links, and a map for the few nodes above level 0
        let mut links0 = vec![NONE; n * M0];
        let mut upper = HashMap::new();
        for (id, ls) in b.links.into_iter().enumerate() {
            let mut ls = ls.into_iter().map(|m| m.into_inner().unwrap());
            let l0 = ls.next().unwrap_or_default();
            links0[id * M0..][..l0.len()].copy_from_slice(&l0);
            let rest: Vec<Vec<u32>> = ls.collect();
            if !rest.is_empty() {
                upper.insert(id as u32, rest);
            }
        }
        let (entry, top) = entry.into_inner().unwrap();
        repair(&mut links0, n, entry, &|a, c| -sp.between(a, c));
        Hnsw { n, links0, upper, entry, top }
    }

    /// The `k` nearest vectors to `q` (by dot product, best first) among `allow` (None: all),
    /// exploring `ef` candidates at level 0. May return fewer than `k` when few nodes are allowed.
    pub(crate) fn search(
        &self,
        sp: &impl Space,
        q: &[f32],
        k: usize,
        ef: usize,
        allow: Option<&RoaringBitmap>,
    ) -> Vec<(u32, f32)> {
        if self.n == 0 || self.entry == NONE {
            return vec![];
        }
        let dist = |o: u32| -sp.to(q, o);
        let fetch = |o: u32| sp.fetch(o);
        let mut eps = vec![(D(dist(self.entry)), self.entry)];
        for l in (1..=self.top).rev() {
            eps = search_level(self, &dist, &fetch, self.n, &eps, 1, l, &|_| true);
        }
        let ok = |id: u32| allow.is_none_or(|a| a.contains(id));
        let found = search_level(self, &dist, &fetch, self.n, &eps, ef.max(k), 0, &ok);
        found.into_iter().take(k).map(|(d, id)| (id, -d.0)).collect()
    }

    /// Nodes no level-0 link points to: a search can reach them only as the entry point.
    pub(crate) fn orphans(&self) -> usize {
        let mut linked = vec![false; self.n];
        for &x in &self.links0 {
            if x != NONE {
                linked[x as usize] = true;
            }
        }
        linked.iter().enumerate().filter(|&(i, &l)| !l && i as u32 != self.entry).count()
    }

    /// The `ef` nearest allowed nodes to a query by the Hamming distance of sign-bit codes
    /// (`words` u64s per node, see `search::signs`). A step costs a few popcounts rather than a
    /// full dot product, so the beam can be wide; the caller rescores at full precision.
    pub(crate) fn candidates(
        &self,
        codes: &[u64],
        words: usize,
        qb: &[u64],
        ef: usize,
        allow: Option<&RoaringBitmap>,
    ) -> Vec<u32> {
        if self.n == 0 || self.entry == NONE {
            return vec![];
        }
        let code = |o: u32| &codes[o as usize * words..][..words];
        let dist = |o: u32| crate::search::hamming(code(o), qb) as f32;
        let fetch = |o: u32| prefetch(code(o));
        let mut eps = vec![(D(dist(self.entry)), self.entry)];
        for l in (1..=self.top).rev() {
            eps = search_level(self, &dist, &fetch, self.n, &eps, 1, l, &|_| true);
        }
        let ok = |id: u32| allow.is_none_or(|a| a.contains(id));
        search_level(self, &dist, &fetch, self.n, &eps, ef, 0, &ok).into_iter().map(|(_, id)| id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(n: usize, dim: usize, seed: u64) -> Vec<f32> {
        let mut x = seed;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        let mut out = vec![];
        for _ in 0..n {
            let v: Vec<f32> = (0..dim).map(|_| next()).collect();
            let norm = v.iter().map(|a| a * a).sum::<f32>().sqrt();
            out.extend(v.iter().map(|a| a / norm));
        }
        out
    }

    fn exact(vecs: &[f32], dim: usize, q: &[f32], k: usize, allow: Option<&RoaringBitmap>) -> Vec<u32> {
        let mut s: Vec<(f32, u32)> = (0..(vecs.len() / dim) as u32)
            .filter(|i| allow.is_none_or(|a| a.contains(*i)))
            .map(|i| (dot(q, &vecs[i as usize * dim..][..dim]), i))
            .collect();
        s.sort_by(|a, b| b.0.total_cmp(&a.0));
        s.into_iter().take(k).map(|(_, i)| i).collect()
    }

    /// Recall@10 of `g` over 100 queries, unfiltered and through a filter of every other row.
    fn recalls(g: &Hnsw, vecs: &[f32], dim: usize) -> [f64; 2] {
        let n = vecs.len() / dim;
        let queries = data(100, dim, 99);
        let half: RoaringBitmap = (0..n as u32).filter(|i| i % 2 == 0).collect();
        [None, Some(&half)].map(|allow| {
            let mut hit = 0;
            for q in queries.chunks(dim) {
                let got: Vec<u32> =
                    g.search(&Floats { vecs, dim }, q, 10, 64, allow).into_iter().map(|(i, _)| i).collect();
                assert!(allow.is_none_or(|a| got.iter().all(|i| a.contains(*i))), "only allowed rows come back");
                let want = exact(vecs, dim, q, 10, allow);
                hit += got.iter().filter(|i| want.contains(i)).count();
            }
            hit as f64 / 1000.0
        })
    }

    #[test]
    fn finds_nearly_all_true_neighbours() {
        let (n, dim) = (5000, 32);
        let vecs = data(n, dim, 7);
        // built on one thread, the graph is the same every run, and so is its recall (0.980, and
        // 0.996 through the filter, on x86-64): the bar holds the graph's quality, with no
        // run-to-run luck in it, and a little room for another machine's float rounding
        let [all, filtered] = recalls(&Hnsw::build_on(&Floats { vecs: &vecs, dim }, 1), &vecs, dim);
        assert!(all > 0.97 && filtered > 0.99, "recall {all}, through the filter {filtered}");
    }

    #[test]
    fn a_build_on_every_core_finds_them_too() {
        let (n, dim) = (5000, 32);
        let vecs = data(n, dim, 7);
        // nodes join in the order the threads take them, so recall varies run to run (0.939 to
        // 0.961 unfiltered on 32 cores, 0.968 to 0.980 through the filter): the bound is below
        // that, to catch a broken concurrent build rather than to measure quality, which the
        // one-thread build above does
        let [all, filtered] = recalls(&Hnsw::build(&Floats { vecs: &vecs, dim }), &vecs, dim);
        assert!(all > 0.9 && filtered > 0.92, "recall {all}, through the filter {filtered}");
    }

    #[test]
    fn empty_and_tiny() {
        let none = Floats { vecs: &[], dim: 4 };
        assert!(Hnsw::build(&none).search(&none, &[1.0, 0.0, 0.0, 0.0], 3, 10, None).is_empty());
        let v = Floats { vecs: &[1.0, 0.0, 0.0, 1.0], dim: 2 };
        let g = Hnsw::build(&v);
        assert_eq!(g.search(&v, &[0.0, 1.0], 1, 10, None)[0].0, 1);
    }
}
