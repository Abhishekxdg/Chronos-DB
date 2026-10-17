//! Prolly tree (Stage 2): a sorted map stored as immutable, content-addressed pages.
//!
//! A node ends after an entry whose key hashes below a threshold (about 1 in 32), or, for a leaf
//! entry of at least `WIDE` bytes, with a chance that grows with its size (see `boundary`), so
//! boundaries depend only on the entries present. The same rows always produce the same pages
//! whatever edits led there, which gives:
//!   - sharing: branches and databases with mostly equal data share mostly the same pages
//!   - diff: subtrees with equal hashes are skipped without being read
//!
//! A tree is named by its root hash. Node bytes:
//!   [u8 level][u32 count] then per entry [u32 len][key] and either [u32 len][value] (leaf, level 0)
//!   or [32-byte child hash] (internal; the key is the child's last key).
//! ponytail: `apply` rebuilds the whole tree, which is CPU O(rows) though only changed pages are
//! written. Switch to cursor-based incremental edits when checkpoints of large trees get slow.

use crate::pages::{Hash, Pages};
use crate::{Change, Val};
use std::collections::HashSet;
use std::io;
use std::sync::Arc;

const AVG_FANOUT: u64 = 32;
const MAX_FANOUT: usize = 1024; // hard cap; hit with probability ~1e-14 per node
/// Leaf entries (key and value) at least this big can also end their leaf by size: narrower rows
/// keep the key-only boundaries, and so the pages, they always had.
const WIDE: usize = 1024;
/// About how many bytes of wide entries a leaf holds. By count alone a leaf of 6 KB rows (1,536
/// numbers each) was about 200 KB, all read, decompressed and hashed to fetch one row: at 1M rows,
/// most of a vector query's 3.4 ms was fetching its 10 hits.
const LEAF_BYTES: u64 = 16 * 1024;

#[derive(Clone)]
enum Kid {
    Val(Val),
    Node(Hash),
}

struct Node {
    level: u8,
    entries: Vec<(String, Kid)>,
}

/// Whether a node ends after the entry with `key` whose key and value take `size` bytes (0 for
/// an internal entry). An entry decides it alone, whatever surrounds it: what lets `apply` reuse
/// untouched subtrees. A wide leaf entry also ends its leaf with chance `size / LEAF_BYTES`, from
/// hash bits the key-only test doesn't use, so leaves of wide rows hold about `LEAF_BYTES`.
fn boundary(level: u8, key: &str, size: usize) -> bool {
    // FNV-1a salted by level, then a splitmix finalizer: cheap and well mixed
    let mut h: u64 = 0xcbf29ce484222325 ^ level as u64;
    for b in key.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d049bb133111eb);
    let h = h ^ (h >> 31);
    #[cfg(test)]
    if tests::KEY_ONLY.get() {
        return h.is_multiple_of(AVG_FANOUT); // the rule before sizes counted, for trees written then
    }
    h.is_multiple_of(AVG_FANOUT) || (level == 0 && size >= WIDE && (h / AVG_FANOUT) % LEAF_BYTES < size as u64)
}

fn encode(level: u8, entries: &[(String, Kid)]) -> Vec<u8> {
    let mut b = vec![level];
    b.extend((entries.len() as u32).to_le_bytes());
    for (k, kid) in entries {
        b.extend((k.len() as u32).to_le_bytes());
        b.extend(k.as_bytes());
        match kid {
            Kid::Val(v) => {
                b.extend((v.len() as u32).to_le_bytes());
                b.extend(v.as_bytes());
            }
            Kid::Node(h) => b.extend(h),
        }
    }
    b
}

fn load(store: &Pages, h: &Hash) -> io::Result<Arc<Node>> {
    store.get_as(h, decode)
}

/// Nodes this thread read lately, a few, by store and page: a thread's reads start from the same few pages (a tree's
/// root and the nodes under it, and a run of rows' leaf), which it then reads without the page cache's lock or a
/// count on the shared page (on many cores at once those made most of a read's time). A page is named by the hash of
/// its contents, so a node kept is never out of date.
const NEAR: usize = 32;

/// The most bytes of entries a leaf kept near holds (a leaf of wide rows is read from the page cache).
const NEAR_LEAF: usize = 8 * 1024;

struct Near {
    store: usize,
    page: Hash,
    node: Arc<Node>,
}

thread_local! {
    static NEAR_NODES: std::cell::RefCell<[Option<Near>; NEAR]> = const { std::cell::RefCell::new([const { None }; NEAR]) };
}

/// `f` on page `h`'s node: one this thread keeps near if it has it, else read (and kept, if `leaves` or it isn't one).
fn with_near<T>(store: &Pages, h: &Hash, leaves: bool, f: impl FnOnce(&Node) -> T) -> io::Result<T> {
    let (at, slot) = (store as *const Pages as usize, usize::from(h[1]) % NEAR);
    let mut f = Some(f);
    let hit = NEAR_NODES.with_borrow(|near| match &near[slot] {
        Some(n) if n.store == at && n.page == *h => f.take().map(|f| f(&n.node)),
        _ => None,
    });
    if let Some(out) = hit {
        store.hits.add(1);
        return Ok(out);
    }
    let node = load(store, h)?;
    let out = f.take().map(|f| f(&node));
    keep(at, slot, h, &node, leaves);
    Ok(out.expect("called once"))
}

/// Keeps a node just read near (see `with_near`): one above leaves, or with `leaves` a leaf of narrow rows.
fn keep(at: usize, slot: usize, h: &Hash, node: &Arc<Node>, leaves: bool) {
    let small = || {
        let sizes = node.entries.iter().map(|(k, kid)| k.len() + if let Kid::Val(v) = kid { v.len() } else { 32 });
        sizes.sum::<usize>() <= NEAR_LEAF
    };
    if node.level > 0 || (leaves && small()) {
        NEAR_NODES.with_borrow_mut(|near| {
            // (a leaf doesn't put out a node above leaves: those are what most reads go through)
            if node.level > 0 || near[slot].as_ref().is_none_or(|n| n.node.level == 0) {
                near[slot] = Some(Near { store: at, page: *h, node: node.clone() });
            }
        });
    }
}

/// Page `h`'s node, through the nodes this thread keeps near (see `with_near`); a leaf isn't kept.
fn near(store: &Pages, h: &Hash) -> io::Result<Arc<Node>> {
    let (at, slot) = (store as *const Pages as usize, usize::from(h[1]) % NEAR);
    let hit = NEAR_NODES.with_borrow(|near| match &near[slot] {
        Some(n) if n.store == at && n.page == *h => Some(n.node.clone()),
        _ => None,
    });
    if let Some(n) = hit {
        store.hits.add(1);
        return Ok(n);
    }
    let node = load(store, h)?;
    keep(at, slot, h, &node, false);
    Ok(node)
}

/// A node and about how much memory it takes: its bytes, each entry's slot, and the allocator's
/// share of its key and value.
fn decode(page: &[u8]) -> io::Result<(Node, usize)> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed tree node");
    let mut r = Reader(page);
    let level = r.take(1).ok_or_else(bad)?[0];
    let n = r.u32().ok_or_else(bad)?;
    let mut entries = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let key = r.str().ok_or_else(bad)?;
        let kid = if level == 0 {
            Kid::Val(r.str().ok_or_else(bad)?.into())
        } else {
            Kid::Node(r.take(32).ok_or_else(bad)?.try_into().unwrap())
        };
        entries.push((key, kid));
    }
    if !r.0.is_empty() {
        return Err(bad());
    }
    let size = page.len() + entries.len() * (size_of::<(String, Kid)>() + if level == 0 { 32 } else { 16 });
    Ok((Node { level, entries }, size))
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (a, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(a)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

/// Builds a tree bottom-up from entries in strictly increasing key order.
struct Builder<'a> {
    store: &'a Pages,
    levels: Vec<Vec<(String, Kid)>>,
}

impl<'a> Builder<'a> {
    fn new(store: &'a Pages) -> Self {
        Builder { store, levels: vec![vec![]] }
    }

    fn push(&mut self, level: usize, key: String, kid: Kid) -> io::Result<()> {
        while self.levels.len() <= level {
            self.levels.push(vec![]);
        }
        let size = match &kid {
            Kid::Val(v) => key.len() + v.len(),
            Kid::Node(_) => 0,
        };
        let end = boundary(level as u8, &key, size);
        self.levels[level].push((key, kid));
        if end || self.levels[level].len() >= MAX_FANOUT {
            self.flush(level)?;
        }
        Ok(())
    }

    fn flush(&mut self, level: usize) -> io::Result<()> {
        let entries = std::mem::take(&mut self.levels[level]);
        let h = self.store.put(&encode(level as u8, &entries))?;
        let last = entries.into_iter().next_back().unwrap().0;
        self.push(level + 1, last, Kid::Node(h))
    }

    fn finish(mut self) -> io::Result<Hash> {
        let mut i = 0;
        loop {
            let top = self.levels[i + 1..].iter().all(|l| l.is_empty());
            if top {
                match &self.levels[i][..] {
                    [] => return self.store.put(&encode(0, &[])), // empty tree
                    [(_, Kid::Node(h))] => return Ok(*h),         // a lone child is the root
                    _ => {}
                }
            }
            if !self.levels[i].is_empty() {
                self.flush(i)?;
            }
            i += 1;
        }
    }
}

/// Stores a tree holding `entries` (strictly increasing keys) and returns its root.
pub fn build(store: &Pages, entries: impl IntoIterator<Item = (String, Val)>) -> io::Result<Hash> {
    let mut b = Builder::new(store);
    for (k, v) in entries {
        b.push(0, k, Kid::Val(v))?;
    }
    b.finish()
}

thread_local! {
    /// The leaf this thread's last `get` ended in, by store and tree: the next often wants a key beside it (the rows
    /// an index finds in its order, when that is about their keys' order too).
    static FINGER: std::cell::Cell<Option<(usize, Hash, Hash)>> = const { std::cell::Cell::new(None) };
}

pub fn get(store: &Pages, root: &Hash, key: &str) -> io::Result<Option<Val>> {
    enum Step {
        Down(Hash),
        Found(Option<Val>),
    }
    let at = store as *const Pages as usize;
    // a key between the first and last of the leaf the last lookup in this tree ended in is in it, or nowhere
    if let Some((s, r, leaf)) = FINGER.get()
        && s == at
        && r == *root
    {
        let found = with_near(store, &leaf, true, |node| {
            let (first, last) = (&node.entries.first()?.0, &node.entries.last()?.0);
            if node.level > 0 || key < first.as_str() || key > last.as_str() {
                return None;
            }
            let i = node.entries.partition_point(|(k, _)| k.as_str() < key);
            Some(match node.entries.get(i) {
                Some((k, Kid::Val(v))) if k == key => Some(v.clone()),
                _ => None,
            })
        })?;
        if let Some(v) = found {
            return Ok(v);
        }
    }
    let mut page = *root;
    loop {
        let step = with_near(store, &page, true, |node| {
            // first entry whose key is >= `key`: for internal nodes, the child that could hold it
            let i = node.entries.partition_point(|(k, _)| k.as_str() < key);
            match node.entries.get(i) {
                None => Step::Found(None),
                Some((_, Kid::Node(h))) => Step::Down(*h),
                Some((k, Kid::Val(v))) => Step::Found((k == key).then(|| v.clone())),
            }
        })?;
        match step {
            Step::Down(h) => page = h,
            Step::Found(v) => {
                FINGER.set(Some((at, *root, page)));
                return Ok(v);
            }
        }
    }
}

/// About how many rows a leaf holds, from the first leaf at or after `from`: `AVG_FANOUT`, or
/// fewer where its entries are wide enough to end leaves by size (see `boundary`).
fn leaf_rows(store: &Pages, root: &Hash, from: &str) -> io::Result<f64> {
    let mut node = load(store, root)?;
    while node.level > 0 {
        let i = node.entries.partition_point(|(k, _)| k.as_str() < from).min(node.entries.len().saturating_sub(1));
        let Some((_, Kid::Node(h))) = node.entries.get(i) else { break };
        node = load(store, h)?;
    }
    let sizes = node.entries.iter().map(|(k, kid)| k.len() + if let Kid::Val(v) = kid { v.len() } else { 0 });
    let (n, bytes) = sizes.fold((0usize, 0usize), |(n, b), s| (n + 1, b + s));
    let avg = if n == 0 { 0.0 } else { bytes as f64 / n as f64 };
    if avg < WIDE as f64 {
        return Ok(AVG_FANOUT as f64);
    }
    let by_key = 1.0 / AVG_FANOUT as f64;
    let end = by_key + (1.0 - by_key) * (avg / LEAF_BYTES as f64).min(1.0); // chance an entry ends its leaf
    Ok(1.0 / end)
}

/// Keys that cut the keys in [from, upto) into runs of about `rows` rows each, each with about
/// how many rows the run ending at it holds. Reads internal nodes, and one leaf for how many rows
/// leaves hold, down to the highest level whose subtrees hold at most `rows` rows, then joins
/// neighbours up to `rows`: so runs stay that size however big the range is (a fixed number of
/// subtrees instead would make them grow with it, 32 times per level).
pub fn marks(store: &Pages, root: &Hash, from: &str, upto: &str, rows: usize) -> io::Result<Vec<(String, usize)>> {
    let mut nodes = vec![load(store, root)?];
    if nodes[0].level == 0 {
        return Ok(vec![]); // one leaf: nothing to cut
    }
    let per_leaf = leaf_rows(store, root, from)?;
    loop {
        let level = nodes[0].level;
        if level == 0 {
            return Ok(vec![]); // one leaf: nothing to cut
        }
        // the subtrees overlapping the range: each holds the keys after the one before it, to its own
        let mut kids: Vec<(&String, Hash)> = vec![];
        let mut prev: Option<&str> = None;
        for n in &nodes {
            for (k, kid) in &n.entries {
                if let Kid::Node(h) = kid
                    && k.as_str() >= from
                    && prev.is_none_or(|p| p < upto)
                {
                    kids.push((k, *h));
                }
                prev = Some(k);
            }
        }
        // rows in each, about: leaves hold `per_leaf`, and each level above has `AVG_FANOUT` children
        let per = ((per_leaf * (AVG_FANOUT as f64).powi(level as i32 - 1)) as usize).max(1);
        if level == 1 || per <= rows || kids.is_empty() {
            let step = (rows / per).max(1);
            let (mut out, mut run) = (vec![], 0);
            for (i, (k, _)) in kids.into_iter().filter(|(k, _)| k.as_str() < upto).enumerate() {
                run += per;
                if (i + 1) % step == 0 {
                    out.push((k.clone(), run));
                    run = 0;
                }
            }
            return Ok(out);
        }
        nodes = kids.iter().map(|(_, h)| load(store, h)).collect::<io::Result<_>>()?;
    }
}

/// Calls `f` on the entries with key >= `from`, in key order, until it returns false (then this
/// does too). Like `range`, but lends each entry instead of copying it.
pub fn visit<E: From<io::Error>>(
    store: &Pages,
    root: &Hash,
    from: &str,
    f: &mut dyn FnMut(&str, &Val) -> Result<bool, E>,
) -> Result<bool, E> {
    let node = near(store, root)?;
    let i = node.entries.partition_point(|(k, _)| k.as_str() < from);
    for (k, kid) in &node.entries[i..] {
        let go = match kid {
            Kid::Val(v) => f(k, v)?,
            Kid::Node(h) => visit(store, h, from, f)?,
        };
        if !go {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Entries with key <= `upto`, in descending key order, until `f` returns false (false then).
/// A branch's key is its child's largest, so the child holding `upto` is the first whose key
/// reaches it; every child before it lies wholly below.
pub fn visit_rev<E: From<io::Error>>(
    store: &Pages,
    root: &Hash,
    upto: &str,
    f: &mut dyn FnMut(&str, &Val) -> Result<bool, E>,
) -> Result<bool, E> {
    let node = near(store, root)?;
    let end = if node.level > 0 {
        (node.entries.partition_point(|(k, _)| k.as_str() < upto) + 1).min(node.entries.len())
    } else {
        node.entries.partition_point(|(k, _)| k.as_str() <= upto)
    };
    for (k, kid) in node.entries[..end].iter().rev() {
        let go = match kid {
            Kid::Val(v) => f(k, v)?,
            Kid::Node(h) => visit_rev(store, h, upto, f)?,
        };
        if !go {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Entries with key >= `from`, in key order, reading pages only as it goes.
pub struct Iter<'a> {
    store: &'a Pages,
    stack: Vec<(Arc<Node>, usize)>,
}

pub fn range<'a>(store: &'a Pages, root: &Hash, from: &str) -> io::Result<Iter<'a>> {
    let mut stack = vec![];
    let mut node = load(store, root)?;
    loop {
        let i = node.entries.partition_point(|(k, _)| k.as_str() < from);
        let child = match node.entries.get(i) {
            Some((_, Kid::Node(h))) if node.level > 0 => Some(*h),
            _ => None,
        };
        stack.push((node, i));
        match child {
            Some(h) => node = load(store, &h)?,
            None => return Ok(Iter { store, stack }),
        }
    }
}

impl Iterator for Iter<'_> {
    type Item = io::Result<(String, Val)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (node, i) = self.stack.last_mut()?;
            let Some((k, kid)) = node.entries.get(*i) else {
                self.stack.pop();
                if let Some((_, i)) = self.stack.last_mut() {
                    *i += 1;
                }
                continue;
            };
            match kid {
                Kid::Val(v) => {
                    let out = (k.clone(), v.clone());
                    *i += 1;
                    return Some(Ok(out));
                }
                Kid::Node(h) => match load(self.store, h) {
                    Ok(child) => self.stack.push((child, 0)),
                    Err(e) => {
                        self.stack.clear();
                        return Some(Err(e));
                    }
                },
            }
        }
    }
}

/// Applies changes (sorted by key; `None` deletes) and returns the new root: exactly the tree
/// `build` would produce for the resulting rows, but it only reads and rewrites the paths the
/// changes touch.
///
/// Why reuse is safe: boundaries depend only on each entry, so a builder that has just closed a node
/// and is fed an old, unchanged subtree reproduces that subtree exactly, so it can take its hash
/// whole. The exception is the old tree's rightmost spine, whose nodes were closed by the end
/// of the data rather than by a boundary; those are always re-read.
pub fn apply(store: &Pages, root: &Hash, changes: &[(String, Option<Val>)]) -> io::Result<Hash> {
    let node = load(store, root)?;
    let max = node.entries.last().map(|(k, _)| k.clone()); // an internal key is its child's last key
    let mut b = Builder::new(store);
    let mut ci = 0;
    walk(store, &mut b, node, changes, &mut ci, max.as_deref())?;
    for (k, v) in &changes[ci..] {
        if let Some(v) = v {
            b.push(0, k.clone(), Kid::Val(v.clone()))?;
        }
    }
    b.finish()
}

fn walk(
    store: &Pages,
    b: &mut Builder,
    node: Arc<Node>,
    changes: &[(String, Option<Val>)],
    ci: &mut usize,
    max: Option<&str>,
) -> io::Result<()> {
    let level = node.level as usize;
    for (key, kid) in &node.entries {
        let key = key.clone();
        match kid.clone() {
            Kid::Val(v) => {
                while let Some((ck, cv)) = changes.get(*ci).filter(|(ck, _)| *ck < key) {
                    if let Some(cv) = cv {
                        b.push(0, ck.clone(), Kid::Val(cv.clone()))?; // insert
                    }
                    *ci += 1;
                }
                match changes.get(*ci).filter(|(ck, _)| *ck == key) {
                    Some((_, cv)) => {
                        *ci += 1;
                        if let Some(cv) = cv {
                            b.push(0, key, Kid::Val(cv.clone()))?; // replace; None deletes
                        }
                    }
                    None => b.push(0, key, Kid::Val(v))?,
                }
            }
            Kid::Node(h) => {
                let untouched = changes.get(*ci).is_none_or(|(ck, _)| *ck > key);
                let closed = Some(key.as_str()) != max;
                if untouched && closed && b.levels.iter().take(level).all(Vec::is_empty) {
                    b.push(level, key, Kid::Node(h))?; // reuse the whole subtree
                } else {
                    walk(store, b, load(store, &h)?, changes, ci, max)?;
                }
            }
        }
    }
    Ok(())
}

/// Every page reachable from `roots`. Reads internal nodes only: a level-1 node already names
/// its leaves, and leaves are nearly all of the pages. Shared subtrees are visited once.
pub fn reachable(store: &Pages, roots: &[Hash]) -> io::Result<HashSet<Hash>> {
    let mut seen = HashSet::new();
    let mut stack: Vec<Hash> = roots.iter().copied().filter(|r| seen.insert(*r)).collect();
    while let Some(h) = stack.pop() {
        let node = load(store, &h)?;
        for (_, kid) in &node.entries {
            if let Kid::Node(c) = kid
                && seen.insert(*c)
                && node.level > 1
            {
                stack.push(*c);
            }
        }
    }
    Ok(seen)
}

/// The pages of tree `b` that tree `a` doesn't share, and their bytes as stored: what `b` costs
/// on top of `a`. The same walk as `diff`, so cost follows the size of the change.
/// ponytail: a subtree `b` shares with `a` at a different depth counts as new; exact would need
/// `reachable` over all of `a`.
pub fn fresh(store: &Pages, a: &Hash, b: &Hash) -> io::Result<(usize, u64)> {
    let (mut fa, mut fb) = (vec![(u8::MAX, *a)], vec![(u8::MAX, *b)]); // (level, hash); MAX = unknown
    let (mut pages, mut bytes) = (0, 0);
    let mut count = |h: &Hash| {
        pages += 1;
        bytes += store.size(h).unwrap_or(0);
    };
    loop {
        let ha: HashSet<Hash> = fa.iter().map(|x| x.1).collect();
        let hb: HashSet<Hash> = fb.iter().map(|x| x.1).collect();
        fa.retain(|x| !hb.contains(&x.1));
        fb.retain(|x| !ha.contains(&x.1));
        for f in [&mut fa, &mut fb] {
            for (lvl, h) in f.iter_mut().filter(|(l, _)| *l == u8::MAX) {
                *lvl = load(store, h)?.level;
            }
        }
        let top = fa.iter().chain(&fb).map(|x| x.0).max().unwrap_or(0);
        if top == 0 {
            fb.iter().for_each(|(_, h)| count(h));
            return Ok((pages, bytes));
        }
        // expand the highest nodes one level; each of b's is new, counted once as it opens
        for (f, mine) in [(&mut fa, false), (&mut fb, true)] {
            let mut next = vec![];
            for (lvl, h) in f.drain(..) {
                if lvl != top {
                    next.push((lvl, h));
                    continue;
                }
                if mine {
                    count(&h);
                }
                for (_, kid) in &load(store, &h)?.entries {
                    if let Kid::Node(c) = kid {
                        next.push((u8::MAX, *c));
                    }
                }
            }
            *f = next;
        }
    }
}

/// Row-level differences from tree `a` to tree `b`, in key order (see `diff_from`).
#[cfg(test)]
pub fn diff(store: &Pages, a: &Hash, b: &Hash) -> io::Result<Vec<Change>> {
    diff_from(store, a, b, None)?.collect()
}

/// Row-level differences from tree `a` to tree `b` with key > `after` (all of them if None), in
/// key order, read lazily. Walks both trees top-down and drops every subtree whose hash appears
/// on both sides, or that holds only keys <= `after`, so cost follows the size of the change.
/// Holds only the differing leaves' hashes and one leaf per side, not the changed rows.
pub fn diff_from<'a>(store: &'a Pages, a: &Hash, b: &Hash, after: Option<&str>) -> io::Result<DiffIter<'a>> {
    let (mut fa, mut fb) = (vec![(u8::MAX, *a)], vec![(u8::MAX, *b)]); // (level, hash); MAX = unknown
    let resolve = |f: &mut Vec<(u8, Hash)>| -> io::Result<()> {
        for (lvl, h) in f.iter_mut().filter(|(l, _)| *l == u8::MAX) {
            *lvl = load(store, h)?.level;
        }
        Ok(())
    };
    loop {
        let common: HashSet<Hash> = fa
            .iter()
            .map(|x| x.1)
            .collect::<HashSet<_>>()
            .intersection(&fb.iter().map(|x| x.1).collect())
            .copied()
            .collect();
        fa.retain(|x| !common.contains(&x.1));
        fb.retain(|x| !common.contains(&x.1));
        resolve(&mut fa)?;
        resolve(&mut fb)?;
        let top = fa.iter().chain(&fb).map(|x| x.0).max().unwrap_or(0);
        if top == 0 {
            let side =
                |f: Vec<(u8, Hash)>| Leaves { store, hashes: f.into_iter(), cur: vec![].into_iter(), head: None };
            return Ok(DiffIter { a: side(fa), b: side(fb), after: after.map(String::from) });
        }
        // expand the highest nodes one level, keeping key order; an entry's key is the last key
        // under it, so a child whose key is <= `after` has nothing past the cursor
        for f in [&mut fa, &mut fb] {
            let mut next = vec![];
            for (lvl, h) in f.drain(..) {
                if lvl == top {
                    for (k, kid) in &load(store, &h)?.entries {
                        if let Kid::Node(c) = kid
                            && after.is_none_or(|x| k.as_str() > x)
                        {
                            next.push((top - 1, *c)); // known: loading it just for its level would read every leaf
                        }
                    }
                } else {
                    next.push((lvl, h));
                }
            }
            *f = next;
        }
    }
}

/// The rows of a run of leaves, in key order, one leaf loaded at a time.
struct Leaves<'a> {
    store: &'a Pages,
    hashes: std::vec::IntoIter<(u8, Hash)>,
    cur: std::vec::IntoIter<(String, Val)>,
    head: Option<(String, Val)>,
}

impl Leaves<'_> {
    fn peek(&mut self, after: Option<&str>) -> io::Result<Option<&(String, Val)>> {
        while self.head.is_none() {
            if let Some(r) = self.cur.next() {
                if after.is_none_or(|x| r.0.as_str() > x) {
                    self.head = Some(r);
                }
            } else if let Some((_, h)) = self.hashes.next() {
                let node = load(self.store, &h)?;
                let rows = node.entries.iter().filter_map(|(k, kid)| match kid {
                    Kid::Val(v) => Some((k.clone(), v.clone())),
                    Kid::Node(_) => None,
                });
                self.cur = rows.collect::<Vec<_>>().into_iter();
            } else {
                break;
            }
        }
        Ok(self.head.as_ref())
    }
}

/// See `diff_from`.
pub struct DiffIter<'a> {
    a: Leaves<'a>,
    b: Leaves<'a>,
    after: Option<String>,
}

impl Iterator for DiffIter<'_> {
    type Item = io::Result<Change>;
    fn next(&mut self) -> Option<Self::Item> {
        let after = self.after.as_deref();
        loop {
            let ka = match self.a.peek(after) {
                Ok(r) => r.map(|r| r.0.clone()),
                Err(e) => return Some(Err(e)),
            };
            let kb = match self.b.peek(after) {
                Ok(r) => r.map(|r| r.0.clone()),
                Err(e) => return Some(Err(e)),
            };
            let c = match (ka, kb) {
                (None, None) => return None,
                (Some(ka), Some(kb)) if ka == kb => {
                    let ((k, x), (_, y)) = (self.a.head.take().unwrap(), self.b.head.take().unwrap());
                    if x == y {
                        continue;
                    }
                    Change { key: k, before: Some(x), after: Some(y) }
                }
                (Some(ka), kb) if kb.as_ref().is_none_or(|kb| &ka < kb) => {
                    let (k, x) = self.a.head.take().unwrap();
                    Change { key: k, before: Some(x), after: None }
                }
                _ => {
                    let (k, y) = self.b.head.take().unwrap();
                    Change { key: k, before: None, after: Some(y) }
                }
            };
            return Some(Ok(c));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    thread_local! {
        /// Builds trees as versions before `WIDE` did: leaves end by key alone.
        pub(super) static KEY_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn store(name: &str) -> (Pages, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("chronos-prolly-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        (Pages::open(&d, None, None).unwrap(), d)
    }

    fn all(s: &Pages, root: &Hash) -> Vec<(String, Val)> {
        range(s, root, "").unwrap().map(Result::unwrap).collect()
    }

    fn naive_diff(a: &BTreeMap<String, Val>, b: &BTreeMap<String, Val>) -> Vec<Change> {
        let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
        keys.into_iter()
            .filter(|k| a.get(*k) != b.get(*k))
            .map(|k| Change { key: k.clone(), before: a.get(k).cloned(), after: b.get(k).cloned() })
            .collect()
    }

    #[test]
    fn matches_btreemap_under_random_edits() {
        let (s, dir) = store("model");
        let mut model = BTreeMap::new();
        let mut root = build(&s, []).unwrap();
        let mut r = Rng(42);
        for round in 0..30 {
            let before = model.clone();
            let before_root = root;
            let mut batch = BTreeMap::new();
            for _ in 0..1 + r.below(if round % 5 == 0 { 3000 } else { 40 }) {
                let k = format!("k{:05}", r.below(5000));
                let v = (r.below(4) != 0).then(|| Val::from(format!("v{}", r.below(100))));
                batch.insert(k, v);
            }
            for (k, v) in &batch {
                match v {
                    Some(v) => model.insert(k.clone(), v.clone()),
                    None => model.remove(k),
                };
            }
            let changes: Vec<_> = batch.into_iter().collect();
            root = apply(&s, &root, &changes).unwrap();
            let rebuilt = build(&s, model.iter().map(|(k, v)| (k.clone(), v.clone()))).unwrap();
            assert_eq!(root, rebuilt, "round {round}: incremental apply must equal a full rebuild");

            assert_eq!(all(&s, &root), model.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
            for _ in 0..50 {
                let k = format!("k{:05}", r.below(5100));
                assert_eq!(get(&s, &root, &k).unwrap(), model.get(&k).cloned(), "get {k}");
                let from: Vec<_> = range(&s, &root, &k).unwrap().take(5).map(Result::unwrap).collect();
                let want: Vec<_> = model.range(k.clone()..).take(5).map(|(k, v)| (k.clone(), v.clone())).collect();
                assert_eq!(from, want, "range from {k}");
            }
            assert_eq!(diff(&s, &before_root, &root).unwrap(), naive_diff(&before, &model), "round {round}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn appends_and_tail_deletes_match_a_rebuild() {
        // sequential ids: every batch lands past the old last key, where the tree's right edge
        // was closed by the end of the data, not by a boundary, and must not be reused
        let (s, dir) = store("append");
        let mut model: BTreeMap<String, Val> = BTreeMap::new();
        let mut root = build(&s, []).unwrap();
        let mut next = 0;
        let mut r = Rng(7);
        for round in 0..60 {
            let mut changes = vec![];
            for _ in 0..1 + r.below(200) {
                changes.push((format!("id{next:07}"), Some(Val::from("v"))));
                next += 1;
            }
            if round % 7 == 3 {
                // trim the tail too
                for (k, _) in model.iter().rev().take(r.below(50) as usize) {
                    changes.push((k.clone(), None));
                }
                changes.sort_by(|a: &(String, _), b| a.0.cmp(&b.0));
            }
            for (k, v) in &changes {
                match v {
                    Some(v) => model.insert(k.clone(), v.clone()),
                    None => model.remove(k),
                };
            }
            root = apply(&s, &root, &changes).unwrap();
            let rebuilt = build(&s, model.iter().map(|(k, v)| (k.clone(), v.clone()))).unwrap();
            assert_eq!(root, rebuilt, "round {round}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn same_rows_same_root_whatever_the_history() {
        let (s, dir) = store("history");
        let rows: Vec<(String, Val)> = (0..3000).map(|i| (format!("k{i:05}"), Val::from(format!("v{i}")))).collect();
        let direct = build(&s, rows.clone()).unwrap();
        // reach the same rows by a different path: build evens, add odds, add junk, delete junk
        let evens = build(&s, rows.iter().filter(|(k, _)| k.ends_with(['0', '2', '4', '6', '8'])).cloned()).unwrap();
        let odds: Vec<_> = rows
            .iter()
            .filter(|(k, _)| !k.ends_with(['0', '2', '4', '6', '8']))
            .map(|(k, v)| (k.clone(), Some(v.clone())))
            .collect();
        let mut root = apply(&s, &evens, &odds).unwrap();
        let junk: Vec<_> = (0..500).map(|i| (format!("k{i:05}x"), Some(Val::from("junk")))).collect();
        root = apply(&s, &root, &junk).unwrap();
        let unjunk: Vec<_> = junk.into_iter().map(|(k, _)| (k, None)).collect();
        root = apply(&s, &root, &unjunk).unwrap();
        assert_eq!(root, direct);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_change_writes_a_handful_of_pages() {
        let (s, dir) = store("share");
        let rows: Vec<(String, Val)> =
            (0..100_000).map(|i| (format!("k{i:06}"), Val::from(format!("value {i}")))).collect();
        let root = build(&s, rows).unwrap();
        let before = s.len();
        let edited = apply(&s, &root, &[("k050000".into(), Some(Val::from("changed")))]).unwrap();
        let new_pages = s.len() - before;
        assert!(new_pages <= 5, "{new_pages} new pages for a one-row change in {before}");
        assert_eq!(diff(&s, &root, &edited).unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pages_survive_reopen_and_corruption_is_caught() {
        let (s, dir) = store("reopen");
        let root = build(&s, (0..2000).map(|i| (format!("k{i:04}"), Val::from("v")))).unwrap();
        s.sync().unwrap();
        drop(s);
        let s = Pages::open(&dir, None, None).unwrap();
        assert_eq!(all(&s, &root).len(), 2000);
        drop(s);

        // a torn append at the end of the pack is dropped on open
        let pack = dir.join("pack-00000000");
        let mut f = std::fs::OpenOptions::new().append(true).open(&pack).unwrap();
        std::io::Write::write_all(&mut f, &[7; 50]).unwrap();
        drop(f);
        let s = Pages::open(&dir, None, None).unwrap();
        assert_eq!(all(&s, &root).len(), 2000);
        drop(s);

        // flipped bits inside a page are detected, never returned
        let mut b = std::fs::read(&pack).unwrap();
        let mid = b.len() / 2;
        b[mid] ^= 0xff;
        std::fs::write(&pack, b).unwrap();
        let s = Pages::open(&dir, None, None).unwrap();
        let err = range(&s, &root, "").unwrap().find_map(Result::err);
        assert!(err.is_some_and(|e| e.kind() == io::ErrorKind::InvalidData));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Scans split tables at marks: their runs must stay about the size asked for however big
    /// the table, not a fixed number of runs (which made each run 32 times bigger per level).
    #[test]
    fn marks_cut_runs_of_about_the_rows_asked_for_at_any_size() {
        let (s, dir) = store("marks");
        for n in [3_000usize, 60_000, 400_000] {
            let root = build(&s, (0..n).map(|i| (format!("t/{i:07}"), Val::from("v")))).unwrap();
            let m = marks(&s, &root, "t/", "t/\u{10ffff}", 4096).unwrap();
            if n < 4096 {
                assert!(m.is_empty(), "{n} rows: one run, got {} marks", m.len());
                continue;
            }
            assert!(m.windows(2).all(|w| w[0].0 < w[1].0), "marks come in key order");
            // the rows each run really holds, from the keys' numbers
            let at: Vec<usize> = m.iter().map(|(k, _)| k[2..].parse::<usize>().unwrap() + 1).collect();
            let runs: Vec<usize> = std::iter::once(at[0]).chain(at.windows(2).map(|w| w[1] - w[0])).collect();
            let avg = runs.iter().sum::<usize>() / runs.len();
            assert!((1500..=10_000).contains(&avg), "{n} rows: runs average {avg} rows");
            assert!(runs.iter().all(|&r| r <= 40_000), "{n} rows: a run of {} rows", runs.iter().max().unwrap());
            assert!(m.iter().all(|(_, est)| (1024..=8192).contains(est)), "{n} rows: estimates {:?}", &m[..3]);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Every leaf of a tree, in key order, as (entries, bytes of keys and values).
    fn leaves(s: &Pages, root: &Hash) -> Vec<(usize, usize)> {
        let node = load(s, root).unwrap();
        if node.level == 0 {
            let bytes = node.entries.iter().map(|(k, kid)| k.len() + if let Kid::Val(v) = kid { v.len() } else { 0 });
            return vec![(node.entries.len(), bytes.sum())];
        }
        node.entries.iter().flat_map(|(_, kid)| if let Kid::Node(h) = kid { leaves(s, h) } else { vec![] }).collect()
    }

    /// Wide rows (here 6 KB, a 1,536-number vector) fill leaves of about `LEAF_BYTES`, not 32 rows
    /// (200 KB), so fetching one reads a leaf that size; narrow rows keep 32 to a leaf.
    #[test]
    fn wide_rows_fill_leaves_by_size() {
        let (s, dir) = store("wide");
        let wide = build(&s, (0..4000).map(|i| (format!("t/{i:06}"), Val::from("x".repeat(6000))))).unwrap();
        let l = leaves(&s, &wide);
        let (rows, bytes): (usize, usize) = l.iter().fold((0, 0), |(r, b), x| (r + x.0, b + x.1));
        let avg = bytes / l.len();
        assert_eq!(rows, 4000);
        assert!((8_000..=32_000).contains(&avg), "wide leaves average {avg} bytes ({} rows each)", rows / l.len());
        let narrow = build(&s, (0..40_000).map(|i| (format!("t/{i:06}"), Val::from("x".repeat(100))))).unwrap();
        let l = leaves(&s, &narrow);
        let per = 40_000 / l.len();
        assert!((24..=40).contains(&per), "narrow leaves hold {per} rows");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Entries under `WIDE` bytes end nodes exactly where the key alone did, so the pages of narrow
    /// tables are the ones every earlier version wrote.
    #[test]
    fn narrow_entries_keep_key_only_boundaries() {
        for i in 0..20_000 {
            let k = format!("t/{i:06}");
            for level in [0u8, 1, 2] {
                let key_only = boundary(level, &k, 0);
                assert_eq!(boundary(level, &k, WIDE - 1), key_only, "{k} at level {level}");
                assert!(!key_only || boundary(level, &k, 50_000), "a key boundary holds at any size");
                if level > 0 {
                    assert_eq!(boundary(level, &k, 50_000), key_only, "internal nodes end by key alone");
                }
            }
        }
    }

    /// Incremental edits give the tree a rebuild gives while values grow past `WIDE` and shrink
    /// back, which moves leaf boundaries without the keys changing.
    #[test]
    fn values_changing_width_match_a_rebuild() {
        let (s, dir) = store("width");
        let mut model = BTreeMap::new();
        let mut root = build(&s, []).unwrap();
        let mut r = Rng(99);
        for round in 0..40 {
            let mut batch = BTreeMap::new();
            for _ in 0..1 + r.below(if round % 6 == 0 { 800 } else { 30 }) {
                let k = format!("k{:05}", r.below(3000));
                let v = match r.below(5) {
                    0 => None,
                    1 | 2 => Some(Val::from("n".repeat(1 + r.below(200) as usize))),
                    _ => Some(Val::from("w".repeat(WIDE + r.below(20_000) as usize))),
                };
                batch.insert(k, v);
            }
            for (k, v) in &batch {
                match v {
                    Some(v) => model.insert(k.clone(), v.clone()),
                    None => model.remove(k),
                };
            }
            let changes: Vec<_> = batch.into_iter().collect();
            root = apply(&s, &root, &changes).unwrap();
            let rebuilt = build(&s, model.iter().map(|(k, v)| (k.clone(), v.clone()))).unwrap();
            assert_eq!(root, rebuilt, "round {round}: incremental apply must equal a full rebuild");
            for _ in 0..20 {
                let k = format!("k{:05}", r.below(3100));
                assert_eq!(get(&s, &root, &k).unwrap(), model.get(&k).cloned(), "get {k}");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Scans of wide tables split into runs of about the rows asked for too: `marks` counts rows
    /// per leaf from a leaf, not 32.
    #[test]
    fn marks_cut_wide_tables_into_runs_of_about_the_rows_asked_for() {
        let (s, dir) = store("wide-marks");
        let n = 60_000;
        let root = build(&s, (0..n).map(|i| (format!("t/{i:07}"), Val::from("x".repeat(6000))))).unwrap();
        let m = marks(&s, &root, "t/", "t/\u{10ffff}", 4096).unwrap();
        let at: Vec<usize> = m.iter().map(|(k, _)| k[2..].parse::<usize>().unwrap() + 1).collect();
        let runs: Vec<usize> = std::iter::once(at[0]).chain(at.windows(2).map(|w| w[1] - w[0])).collect();
        let avg = runs.iter().sum::<usize>() / runs.len();
        assert!((1500..=10_000).contains(&avg), "runs average {avg} rows");
        let est: usize = m.iter().map(|x| x.1).sum::<usize>() + m[0].1;
        assert!((n / 2..=n * 2).contains(&est), "{est} rows estimated for {n}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A database written before sizes counted holds trees whose wide leaves end by key alone.
    /// Every such end is also an end under the new rule (a key boundary holds at any size), so
    /// edits made now reuse its untouched subtrees: they read and diff exactly, and two worlds
    /// forked from it share its pages, so a diff between them reads only what either changed.
    /// An edit to an old wide leaf rewrites it as several smaller ones: more pages than on a tree
    /// built now, about the bytes the rule before wrote for the same edit.
    #[test]
    fn trees_from_before_sizes_counted_edit_and_diff_as_before() {
        let (s, dir) = store("mixed");
        let wide = |i: u64, salt: u64| Val::from(format!("{i}-{salt}-").repeat(1 + 3000 / (8 + i.to_string().len())));
        let rows: BTreeMap<String, Val> = (0..20_000u64).map(|i| (format!("t/{i:06}"), wide(i, 0))).collect();
        KEY_ONLY.set(true);
        let old = build(&s, rows.iter().map(|(k, v)| (k.clone(), v.clone()))).unwrap();
        KEY_ONLY.set(false);
        let new = build(&s, rows.iter().map(|(k, v)| (k.clone(), v.clone()))).unwrap();
        assert_ne!(old, new, "wide leaves end in other places now");
        let mut r = Rng(5);
        let mut edits = |salt: u64| -> Vec<(String, Option<Val>)> {
            let mut b = BTreeMap::new();
            for _ in 0..50 {
                let i = r.below(20_000);
                b.insert(format!("t/{i:06}"), (r.below(5) != 0).then(|| wide(i, salt)));
            }
            b.into_iter().collect()
        };
        let (ea, eb) = (edits(1), edits(2));
        let model = |e: &[(String, Option<Val>)]| {
            let mut m = rows.clone();
            for (k, v) in e {
                match v {
                    Some(v) => m.insert(k.clone(), v.clone()),
                    None => m.remove(k),
                };
            }
            m
        };
        let (ma, mb) = (model(&ea), model(&eb));
        // two worlds forked from the old tree, each edited now
        let a = apply(&s, &old, &ea).unwrap();
        let b = apply(&s, &old, &eb).unwrap();
        let pairs = |m: &BTreeMap<String, Val>| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>();
        assert_eq!(all(&s, &a), pairs(&ma));
        assert_eq!(all(&s, &b), pairs(&mb));
        for (k, v) in ma.iter().step_by(97) {
            assert_eq!(get(&s, &a, k).unwrap().as_ref(), Some(v), "get {k}");
        }
        assert_eq!(diff(&s, &old, &a).unwrap(), naive_diff(&rows, &ma));
        assert_eq!(diff(&s, &a, &b).unwrap(), naive_diff(&ma, &mb));
        // the same rows in a tree built now: equal rows, other pages, and a diff that finds none
        let fresh_a = build(&s, ma.iter().map(|(k, v)| (k.clone(), v.clone()))).unwrap();
        assert!(diff(&s, &a, &fresh_a).unwrap().is_empty());
        // bytes the edits wrote, and bytes a diff of the two worlds reads, against what the rule
        // before wrote and read for the same edits on the same tree: an edit to an old wide leaf
        // rewrites it whole either way, now as several smaller leaves
        KEY_ONLY.set(true);
        let (a_before, b_before) = (apply(&s, &old, &ea).unwrap(), apply(&s, &old, &eb).unwrap());
        KEY_ONLY.set(false);
        let bytes = |x: &Hash, y: &Hash| fresh(&s, x, y).unwrap().1;
        let (wrote, wrote_before) = (bytes(&old, &a), bytes(&old, &a_before));
        let (read, read_before) = (bytes(&a, &b), bytes(&a_before, &b_before));
        assert!(wrote <= wrote_before + wrote_before / 4 + 65_536, "edits wrote {wrote} bytes, {wrote_before} before");
        assert!(
            read <= read_before + read_before / 4 + 65_536,
            "a diff of the worlds reads {read} bytes, {read_before} before"
        );
        // what the first edits cost in pages, once per old leaf: an old leaf of about 32 rows of
        // 3 KB becomes about six, where a tree built now writes one. 323 against 87 here; more
        // than five times would mean old leaves are being split finer than new ones
        let first = fresh(&s, &old, &a).unwrap().0;
        let now_first = fresh(&s, &new, &apply(&s, &new, &ea).unwrap()).unwrap().0;
        assert!(
            first <= now_first * 5,
            "first edits on old leaves wrote {first} pages, {now_first} on a tree built now"
        );
        // once rewritten, those leaves cost what a tree built now costs: the same keys again
        let again: Vec<(String, Option<Val>)> =
            ea.iter().map(|(k, _)| (k.clone(), Some(wide(k[2..].parse().unwrap(), 3)))).collect();
        let later = bytes(&a, &apply(&s, &a, &again).unwrap());
        let now = bytes(&fresh_a, &apply(&s, &fresh_a, &again).unwrap());
        assert!(later <= now + now / 4 + 65_536, "the same keys again cost {later} bytes, {now} on a tree built now");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
