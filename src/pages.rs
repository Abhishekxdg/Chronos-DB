//! Content-addressed page store (Stage 2). A page is immutable and named by the blake3 hash of
//! its bytes, so identical pages are stored once and the name doubles as the checksum. Pages are
//! LZ4-compressed and appended to pack files, the unit that later moves to object storage.
//!
//! Pack record: [32-byte hash][u32 raw len][u32 compressed len][compressed bytes].
//! A record cut short at the end of the last pack is a crash mid-append and is truncated on open.
//! A finished (sealed) pack gets an index file, `pack-N.idx`, so opening reads indexes instead of
//! scanning packs; only the pack still being written is scanned. With a remote (see `remote`),
//! sealed packs and their indexes are uploaded, and a pack missing locally is downloaded the first
//! time one of its pages is read.
//! Cleanup (`compact`) copies the live pages out of mostly-dead packs and deletes those packs, so
//! pack numbers can have gaps.
//! ponytail: remote packs are fetched whole (64 MB) and kept on local disk; use range reads if
//! local disk is smaller than the working set or packs get large.

use crate::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::sync::{Arc, Mutex, RwLock};
use std::any::Any;
use std::collections::HashSet;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::crypt::Crypt;
use crate::disk::{self, fatal};
use crate::remote::{self, Remote};

pub type Hash = [u8; 32];

const HDR: usize = 40;
const PACK_MAX: u64 = 64 << 20;
/// The largest page a record may claim to decode to.
const PAGE_MAX: u64 = 1 << 30;
/// Decoded pages kept in memory: CHRONOS_CACHE_MB megabytes (default 256). Raise it when the rows
/// queries read (vectors, big JSON) outgrow it and the machine has the memory.
fn cache_bytes() -> usize {
    std::env::var("CHRONOS_CACHE_MB").ok().and_then(|m| m.parse::<usize>().ok()).map_or(256 << 20, |m| m << 20)
}

#[derive(Clone, Copy)]
struct Loc {
    pack: usize,
    off: u64,
    raw: u32,
    comp: u32,
}

struct Writer {
    file: File,
    id: usize,
    len: u64,
    entries: Vec<(Hash, Loc)>, // this pack's records, for its index file
    unsent: BTreeSet<usize>,   // sealed packs the remote doesn't have yet: `sync` uploads them
}

/// Decoded pages, so hot lookups skip both the read and the parse. Sized by the memory each
/// decoded page takes, as its decoder reckons it, and never over `cap` once an insert returns.
/// Eviction is CLOCK (second chance): a hit only sets a flag, so hits stay under the read lock,
/// and a page used since it last came up for eviction goes to the back instead of out.
struct Cache {
    map: HashMap<Hash, Cached, std::hash::BuildHasherDefault<ByName>>,
    order: VecDeque<Hash>,
    bytes: usize,
    cap: usize,
}

struct Cached {
    page: Arc<dyn Any + Send + Sync>,
    size: usize,
    used: AtomicBool,
}

impl Cache {
    fn new(cap: usize) -> Cache {
        Cache { map: HashMap::default(), order: VecDeque::new(), bytes: 0, cap }
    }

    fn insert(&mut self, h: Hash, page: Arc<dyn Any + Send + Sync>, size: usize) {
        if self.map.insert(h, Cached { page, size, used: AtomicBool::new(false) }).is_none() {
            self.bytes += size;
            self.order.push_back(h);
        }
        let mut budget = 2 * self.order.len(); // every entry gets at most one second chance
        while self.bytes > self.cap && budget > 0 {
            budget -= 1;
            let Some(old) = self.order.pop_front() else { break };
            match self.map.get(&old) {
                Some(c) if c.used.swap(false, Ordering::Relaxed) => self.order.push_back(old),
                Some(_) => self.bytes -= self.map.remove(&old).unwrap().size,
                None => {} // already dropped by cleanup
            }
        }
    }

    fn remove(&mut self, h: &Hash) {
        if let Some(c) = self.map.remove(h) {
            self.bytes -= c.size; // its `order` entry is skipped when it comes up
        }
    }
}

/// Hashes a page's name for the cache's map by its first bytes: a name is already a hash of the page.
#[derive(Default)]
struct ByName(u64);

impl std::hash::Hasher for ByName {
    fn write(&mut self, bytes: &[u8]) {
        let mut b = [0u8; 8];
        let n = bytes.len().min(8);
        b[..n].copy_from_slice(&bytes[..n]);
        self.0 = self.0.rotate_left(5) ^ u64::from_le_bytes(b);
    }

    fn write_usize(&mut self, _: usize) {} // (the name's length, the same for every name)

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A count many threads add to, kept on one of several cache lines by thread: on one line, every core's
/// addition waited for the others' (a page cache hit counted from 8 cores at once was most of a read's time).
pub(crate) struct Striped([Line; STRIPES]);

#[repr(align(64))]
struct Line(AtomicU64);

const STRIPES: usize = 16;

impl Striped {
    fn new() -> Striped {
        Striped(std::array::from_fn(|_| Line(AtomicU64::new(0))))
    }

    pub(crate) fn add(&self, n: u64) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        std::thread_local! {
            static STRIPE: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
        }
        let mut at = STRIPE.get();
        if at == usize::MAX {
            at = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % STRIPES;
            STRIPE.set(at);
        }
        self.0[at].0.fetch_add(n, Ordering::Relaxed);
    }

    fn sum(&self) -> u64 {
        self.0.iter().map(|l| l.0.load(Ordering::Relaxed)).sum()
    }
}

pub struct Pages {
    dir: PathBuf,
    // ponytail: every page's location stays in memory, about 90 bytes a page (30 MB for 811 MB of
    // pages in examples/big.rs), twice that while opening; sort each pack's index file by hash
    // and binary-search it on disk instead if databases outgrow memory a hundredfold.
    index: RwLock<HashMap<Hash, Loc>>,
    /// pages the pack still being written held at open, not read since: a crash can leave its
    /// unsynced tail with a record's length but zeros for its bytes, so `put` checks one before
    /// taking it as stored (else the same page, written again, would never be)
    unchecked: Mutex<HashSet<Hash>>,
    packs: RwLock<HashMap<usize, File>>, // read handles, by pack id
    bytes: AtomicU64,                    // total size of all packs
    bytes_after_gc: AtomicU64,
    w: Mutex<Writer>,
    cache: RwLock<Cache>, // hits share the read lock
    pub(crate) hits: Striped,
    misses: AtomicU64,
    remote: Option<(Remote, String)>, // and our lease owner id: every write to it is fenced
    fetching: Mutex<()>,              // one download at a time
    crypt: Option<Arc<Crypt>>,        // pages are named and stored encrypted (see `crypt`)
}

fn pack_name(id: usize) -> String {
    format!("pack-{id:08}")
}

fn pack_path(dir: &Path, id: usize) -> PathBuf {
    dir.join(pack_name(id))
}

fn idx_path(dir: &Path, id: usize) -> PathBuf {
    dir.join(format!("{}.idx", pack_name(id)))
}

/// Index file: per record [hash][u64 offset][u32 raw len][u32 compressed len], then a crc32.
fn encode_idx(entries: &[(Hash, Loc)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(entries.len() * 48 + 4);
    for (h, l) in entries {
        b.extend(h);
        b.extend(l.off.to_le_bytes());
        b.extend(l.raw.to_le_bytes());
        b.extend(l.comp.to_le_bytes());
    }
    b.extend(crc32fast::hash(&b).to_le_bytes());
    b
}

fn decode_idx(path: &Path, pack: usize) -> io::Result<Vec<(Hash, Loc)>> {
    let b = fs::read(path)?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("{} is corrupt", path.display()));
    let (body, crc) = b.split_at_checked(b.len().saturating_sub(4)).ok_or_else(bad)?;
    if crc.len() != 4 || crc32fast::hash(body) != u32::from_le_bytes(crc.try_into().unwrap()) || body.len() % 48 != 0 {
        return Err(bad());
    }
    Ok(body
        .chunks_exact(48)
        .map(|r| {
            let u32_at = |i: usize| u32::from_le_bytes(r[i..i + 4].try_into().unwrap());
            let off = u64::from_le_bytes(r[32..40].try_into().unwrap());
            (r[..32].try_into().unwrap(), Loc { pack, off, raw: u32_at(40), comp: u32_at(44) })
        })
        .collect())
}

pub fn hash(bytes: &[u8]) -> Hash {
    blake3::hash(bytes).into()
}

impl Pages {
    /// `remote` comes with the lease owner id that must still hold the remote for us to write.
    pub fn open(
        dir: impl AsRef<Path>,
        remote: Option<(Remote, String)>,
        crypt: Option<Arc<Crypt>>,
    ) -> io::Result<Pages> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let listed: HashMap<String, u64> = match &remote {
            Some((r, _)) => r.list()?.into_iter().collect(),
            None => HashMap::new(),
        };
        if let Some((r, _)) = &remote {
            set_aside_clashing(&dir, &listed, r)?;
        }
        if let Some((r, _)) = &remote {
            for name in listed.keys().filter(|n| n.starts_with("pack-") && n.ends_with(".idx")) {
                remote::fetch(r, name, &dir.join(name))?; // indexes only; packs come on demand
            }
        }
        let mut ids: Vec<usize> = fs::read_dir(&dir)?
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                name.strip_prefix("pack-")?.trim_end_matches(".idx").parse().ok()
            })
            .collect();
        ids.sort();
        ids.dedup();
        // an index whose pack is gone, here and in the remote: left by a cleanup cut short. Its
        // pages are gone too, and must not look stored (`put` would skip writing them again).
        for &id in &ids {
            let name = pack_name(id);
            if !pack_path(&dir, id).exists() && !listed.contains_key(&name) {
                fs::remove_file(idx_path(&dir, id))?;
                if let Some((r, _)) = &remote {
                    r.delete(&format!("{name}.idx"))?;
                }
            }
        }
        ids.retain(|&id| pack_path(&dir, id).exists() || listed.contains_key(&pack_name(id)));
        let (mut index, mut packs, mut bytes) = (HashMap::new(), HashMap::new(), 0);
        let mut unchecked = HashSet::new();
        let last = ids.last().copied();
        for &id in &ids {
            let path = pack_path(&dir, id);
            if idx_path(&dir, id).exists() {
                index.extend(decode_idx(&idx_path(&dir, id), id)?);
            } else {
                let found = scan(&path, id, Some(id) == last)?;
                if Some(id) != last {
                    // finished, but its index was deleted by a cleanup cut short: index it again
                    disk::write_file(&idx_path(&dir, id), &encode_idx(&found))?;
                } else {
                    unchecked.extend(found.iter().map(|(h, _)| *h));
                }
                index.extend(found);
            }
            if path.exists() {
                bytes += fs::metadata(&path)?.len();
                packs.insert(id, File::open(&path)?);
            }
        }
        // keep appending to an unfinished last pack; otherwise start a new one, numbered after
        // every pack the remote has (one without its index yet included)
        let top = listed.keys().filter_map(|n| n.strip_prefix("pack-")?.parse::<usize>().ok()).max();
        let (id, entries) = match last {
            Some(id) if pack_path(&dir, id).exists() && !idx_path(&dir, id).exists() && top.is_none_or(|t| t < id) => {
                (id, index.iter().filter(|(_, l)| l.pack == id).map(|(h, l)| (*h, *l)).collect())
            }
            _ => (last.max(top).map_or(0, |n| n + 1), vec![]),
        };
        // sealed packs here that the remote lacks (sealed before it was attached, or an upload
        // that failed): the next `sync` uploads them, and until then they stay on local disk
        let unsent = match &remote {
            Some(_) => (ids.iter().copied())
                .filter(|&n| n != id && pack_path(&dir, n).exists())
                .filter(|&n| {
                    !listed.contains_key(&pack_name(n)) || !listed.contains_key(&format!("{}.idx", pack_name(n)))
                })
                .collect(),
            None => BTreeSet::new(),
        };
        let file = OpenOptions::new().create(true).append(true).open(pack_path(&dir, id))?;
        disk::sync_dir(&dir)?; // the new pack's name, durable before any page in it is relied on
        packs.entry(id).or_insert(File::open(pack_path(&dir, id))?);
        let len = file.metadata()?.len();
        Ok(Pages {
            dir,
            index: RwLock::new(index),
            unchecked: Mutex::new(unchecked),
            packs: RwLock::new(packs),
            bytes: AtomicU64::new(bytes),
            bytes_after_gc: AtomicU64::new(bytes),
            w: Mutex::new(Writer { file, id, len, entries, unsent }),
            cache: RwLock::new(Cache::new(cache_bytes())),
            hits: Striped::new(),
            misses: AtomicU64::new(0),
            remote,
            fetching: Mutex::new(()),
            crypt,
        })
    }

    /// Stores a page and returns its name. Storing bytes that already exist writes nothing.
    /// A page's name: the hash of its bytes (keyed, when encrypted).
    fn name(&self, bytes: &[u8]) -> Hash {
        match &self.crypt {
            Some(c) => c.name(bytes),
            None => hash(bytes),
        }
    }

    pub fn put(&self, bytes: &[u8]) -> io::Result<Hash> {
        let h = self.name(bytes);
        if self.stored(&h) {
            return Ok(h);
        }
        let comp = lz4_flex::compress(bytes);
        let comp = match &self.crypt {
            Some(c) => c.seal_page(&h, &comp),
            None => comp,
        };
        let mut w = self.w.lock().unwrap();
        if self.stored(&h) {
            return Ok(h); // another writer stored it while we compressed
        }
        self.write_record(&mut w, &h, bytes.len() as u32, &comp)?;
        self.unchecked.lock().unwrap().remove(&h); // written again, whole
        Ok(h)
    }

    /// Whether page `h` is stored: indexed, and readable if it's one `unchecked` holds (a damaged
    /// one is then written again, and indexed where it's whole).
    fn stored(&self, h: &Hash) -> bool {
        if !self.index.read().unwrap().contains_key(h) {
            return false;
        }
        if !self.unchecked.lock().unwrap().contains(h) {
            return true;
        }
        let whole = self.read(h).is_ok();
        if whole {
            self.unchecked.lock().unwrap().remove(h);
        }
        whole
    }

    /// Appends one record to the current pack and indexes it, starting a new pack when full.
    fn write_record(&self, w: &mut Writer, h: &Hash, raw: u32, comp: &[u8]) -> io::Result<()> {
        let mut rec = Vec::with_capacity(HDR + comp.len());
        rec.extend(h);
        rec.extend(raw.to_le_bytes());
        rec.extend((comp.len() as u32).to_le_bytes());
        rec.extend(comp);
        let wrote = if crate::disk::page_write_fails() {
            let half = w.file.write_all(&rec[..rec.len() / 2]);
            half.and(Err(io::Error::new(io::ErrorKind::StorageFull, "no space left on device (injected)")))
        } else {
            w.file.write_all(&rec)
        };
        if let Err(e) = wrote {
            // part of it may be in the pack (a full disk): cut back to where the records end, or
            // every later record would be read from the wrong place
            let cut = OpenOptions::new().write(true).open(pack_path(&self.dir, w.id)).and_then(|f| f.set_len(w.len));
            cut.unwrap_or_else(|e| fatal(e));
            return Err(e);
        }
        crate::disk::crash_point();
        let loc = Loc { pack: w.id, off: w.len + HDR as u64, raw, comp: comp.len() as u32 };
        w.len += rec.len() as u64;
        w.entries.push((*h, loc));
        self.bytes.fetch_add(rec.len() as u64, Ordering::Relaxed);
        self.index.write().unwrap().insert(*h, loc);
        if w.len >= PACK_MAX {
            self.seal(w)?;
        }
        Ok(())
    }

    /// Finishes the current pack: durable and indexed, then starts the next one. With a remote
    /// it waits in `unsent` for `sync` to upload it.
    fn seal(&self, w: &mut Writer) -> io::Result<()> {
        w.file.sync_all().unwrap_or_else(|e| fatal(e));
        crate::disk::crash_point();
        disk::write_file(&idx_path(&self.dir, w.id), &encode_idx(&w.entries))?;
        crate::disk::crash_point();
        let id = w.id + 1;
        let path = pack_path(&self.dir, id);
        let next = OpenOptions::new().create_new(true).append(true).open(&path).and_then(|f| {
            disk::sync_dir(&self.dir)?;
            Ok((f, File::open(&path)?))
        });
        let (file, read) = match next {
            Ok(f) => f,
            Err(e) => {
                // this pack goes on being written, so it must not look finished
                let _ = fs::remove_file(&path);
                fs::remove_file(idx_path(&self.dir, w.id)).unwrap_or_else(|e| fatal(e));
                return Err(e);
            }
        };
        self.packs.write().unwrap().insert(id, read);
        if self.remote.is_some() {
            w.unsent.insert(w.id);
        }
        w.entries.clear();
        (w.file, w.id, w.len) = (file, id, 0);
        Ok(())
    }

    /// Uploads the sealed packs the remote lacks, each before its index, so a remote index never
    /// names a pack that isn't there.
    fn upload(&self, w: &mut Writer) -> io::Result<()> {
        if w.unsent.is_empty() {
            return Ok(());
        }
        let Some(r) = self.fenced()? else { return Ok(()) };
        while let Some(&id) = w.unsent.first() {
            r.put(&pack_name(id), fs::read(pack_path(&self.dir, id))?)?;
            r.put(&format!("{}.idx", pack_name(id)), fs::read(idx_path(&self.dir, id))?)?;
            w.unsent.remove(&id);
        }
        Ok(())
    }

    /// See `sync`; with the writer held.
    fn sync_locked(&self, w: &mut Writer) -> io::Result<()> {
        if self.remote.is_none() {
            w.file.sync_all().unwrap_or_else(|e| fatal(e));
            return Ok(());
        }
        // what an earlier upload left first: while the remote takes nothing, a checkpoint retried
        // every few seconds must not seal a new small pack each time
        self.upload(w)?;
        if w.len > 0 {
            self.seal(w)?;
        }
        self.upload(w)
    }

    /// Reads a page and decodes it with `decode`, serving repeat reads from the cache. `decode`
    /// returns the page and about how many bytes of memory it takes.
    pub fn get_as<T: Send + Sync + 'static>(
        &self,
        h: &Hash,
        decode: impl FnOnce(&[u8]) -> io::Result<(T, usize)>,
    ) -> io::Result<Arc<T>> {
        if let Some(c) = self.cache.read().unwrap().map.get(h)
            && let Ok(t) = c.page.clone().downcast::<T>()
        {
            // (set only when it isn't: a hot page's flag, written by every reader, would bounce between cores)
            if !c.used.load(Ordering::Relaxed) {
                c.used.store(true, Ordering::Relaxed);
            }
            self.hits.add(1);
            return Ok(t);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let (t, size) = decode(&self.read(h)?)?;
        let t = Arc::new(t);
        self.cache.write().unwrap().insert(*h, t.clone(), size);
        Ok(t)
    }

    /// The page cache's bytes now (never over CHRONOS_CACHE_MB), and its hits and misses so far.
    pub fn cache_stats(&self) -> [(&'static str, u64); 3] {
        [
            ("cache_bytes", self.cache.read().unwrap().bytes as u64),
            ("cache_hits", self.hits.sum()),
            ("cache_misses", self.misses.load(Ordering::Relaxed)),
        ]
    }

    /// Reads a page and checks it against its name; returns its size.
    pub fn check(&self, h: &Hash) -> io::Result<u64> {
        self.read(h).map(|raw| raw.len() as u64)
    }

    /// Reads a page from its pack, verifying its bytes against its name.
    fn read(&self, h: &Hash) -> io::Result<Vec<u8>> {
        let missing = || io::Error::new(io::ErrorKind::NotFound, format!("page {} is missing", hex(h)));
        // Cleanup can move a live page to another pack between the lookup and the read: retry once.
        let mut comp = vec![];
        let mut loc = None;
        for _ in 0..3 {
            let l = *self.index.read().unwrap().get(h).ok_or_else(missing)?;
            if let Some(f) = self.packs.read().unwrap().get(&l.pack) {
                comp = read_rec(f, &l).map_err(|e| io::Error::new(e.kind(), format!("page {}: {e}", hex(h))))?;
                loc = Some(l);
                break;
            }
            self.fetch(l.pack)?; // not here: maybe only in the remote
        }
        let loc = loc.ok_or_else(missing)?;
        decode_page(self.crypt.as_deref(), h, loc.raw, comp)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("page {} is corrupt", hex(h))))
    }

    /// Downloads a pack that only the remote has. A no-op without a remote.
    fn fetch(&self, id: usize) -> io::Result<()> {
        let Some((r, _)) = &self.remote else { return Ok(()) };
        let _one = self.fetching.lock().unwrap();
        if self.packs.read().unwrap().contains_key(&id) {
            return Ok(()); // another reader fetched it
        }
        let path = pack_path(&self.dir, id);
        if remote::fetch(r, &pack_name(id), &path)? {
            self.bytes.fetch_add(fs::metadata(&path)?.len(), Ordering::Relaxed);
            self.packs.write().unwrap().insert(id, File::open(&path)?);
        }
        Ok(())
    }

    /// Makes every page stored so far durable: on local disk, and in the remote if there is one
    /// (sealing the current pack). Call before publishing a root that references them.
    /// A failed fsync is fatal: retrying could report success for pages the failed one dropped.
    pub fn sync(&self) -> io::Result<()> {
        self.sync_locked(&mut self.w.lock().unwrap())
    }

    /// Uploads a small object next to the packs (the manifest). A no-op without a remote.
    pub fn publish(&self, name: &str, bytes: Vec<u8>) -> io::Result<()> {
        match self.fenced()? {
            Some(r) => r.put(name, bytes),
            None => Ok(()),
        }
    }

    pub fn remote(&self) -> Option<&Remote> {
        self.remote.as_ref().map(|(r, _)| r)
    }

    /// The remote, after checking we still hold its lease: a writer that was taken over must not
    /// upload (its pack numbers would collide with the new owner's) or delete anything.
    fn fenced(&self) -> io::Result<Option<&Remote>> {
        match &self.remote {
            Some((r, owner)) => remote::check(r, owner).map(|()| Some(r)),
            None => Ok(None),
        }
    }

    /// Deletes local copies of packs the remote already has; they come back on demand.
    /// Returns bytes freed locally.
    pub fn evict_local(&self) -> io::Result<u64> {
        if self.remote.is_none() {
            return Ok(0);
        }
        let w = self.w.lock().unwrap();
        let mut packs = self.packs.write().unwrap();
        let sealed: Vec<usize> = packs.keys().copied().filter(|id| *id != w.id && !w.unsent.contains(id)).collect();
        let mut freed = 0;
        for id in sealed {
            packs.remove(&id);
            let path = pack_path(&self.dir, id);
            freed += fs::metadata(&path)?.len();
            fs::remove_file(path)?;
        }
        self.bytes.fetch_sub(freed, Ordering::Relaxed);
        Ok(freed)
    }

    /// True once the packs have doubled in size since the last cleanup (and hold at least 16 MB).
    pub fn should_gc(&self) -> bool {
        let (now, then) = (self.bytes.load(Ordering::Relaxed), self.bytes_after_gc.load(Ordering::Relaxed));
        now >= 16 << 20 && now > 2 * then
    }

    /// Forgets every page not in `live`, and rewrites each finished pack that is mostly dead:
    /// its live pages are copied into the current pack and made durable, then it is deleted.
    /// Returns (pages dropped, bytes freed).
    pub fn compact(&self, live: &HashSet<Hash>) -> io::Result<(usize, u64)> {
        let mut w = self.w.lock().unwrap(); // no new pages meanwhile
        let (mut live_bytes, mut dead) = (HashMap::<usize, u64>::new(), vec![]);
        for (h, l) in self.index.read().unwrap().iter() {
            if live.contains(h) {
                *live_bytes.entry(l.pack).or_default() += HDR as u64 + l.comp as u64;
            } else {
                dead.push(*h);
            }
        }
        {
            let mut idx = self.index.write().unwrap();
            let mut c = self.cache.write().unwrap();
            for h in &dead {
                idx.remove(h);
                c.remove(h);
            }
        }
        let sizes: Vec<(usize, u64)> = self
            .packs
            .read()
            .unwrap()
            .iter()
            .map(|(id, f)| Ok((*id, f.metadata()?.len())))
            .collect::<io::Result<_>>()?;
        let victims: Vec<(usize, u64)> =
            sizes.into_iter().filter(|(id, size)| live_bytes.get(id).copied().unwrap_or(0) * 2 < *size).collect();
        if victims.iter().any(|(id, _)| *id == w.id) {
            self.seal(&mut w)?; // live pages get copied into a fresh pack, not back into this one
        }
        let mut copied = 0;
        for &(id, _) in &victims {
            let moving: Vec<(Hash, Loc)> =
                self.index.read().unwrap().iter().filter(|(_, l)| l.pack == id).map(|(h, l)| (*h, *l)).collect();
            for (h, l) in moving {
                let comp = read_rec(&self.packs.read().unwrap()[&id], &l)?;
                self.write_record(&mut w, &h, l.raw, &comp)?;
                copied += HDR as u64 + l.comp as u64;
            }
        }
        // copies are durable (and uploaded) before any original goes
        self.sync_locked(&mut w)?;
        let fenced = if victims.is_empty() { None } else { self.fenced()? };
        let mut freed = 0;
        // indexes first, the remote's before ours, packs after: an index never names a pack
        // that's gone (a pack left without one is indexed again at open, and cleaned up again)
        for (id, size) in victims {
            if let Some(r) = fenced {
                r.delete(&format!("{}.idx", pack_name(id)))?;
            }
            crate::disk::crash_point();
            match fs::remove_file(idx_path(&self.dir, id)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            crate::disk::crash_point();
            self.packs.write().unwrap().remove(&id);
            fs::remove_file(pack_path(&self.dir, id))?;
            if let Some(r) = fenced {
                r.delete(&pack_name(id))?;
            }
            w.unsent.remove(&id);
            freed += size;
        }
        File::open(&self.dir)?.sync_all()?;
        let now = self.bytes.fetch_sub(freed, Ordering::Relaxed) - freed;
        self.bytes_after_gc.store(now, Ordering::Relaxed);
        Ok((dead.len(), freed.saturating_sub(copied)))
    }

    /// Bytes as stored of the pages in `live`, of those only in `kept`, and of all the packs.
    pub fn split(&self, live: &HashSet<Hash>, kept: &HashSet<Hash>) -> (u64, u64, u64) {
        let (mut a, mut b) = (0, 0);
        for (h, l) in self.index.read().unwrap().iter() {
            let n = HDR as u64 + l.comp as u64;
            if live.contains(h) {
                a += n;
            } else if kept.contains(h) {
                b += n;
            }
        }
        (a, b, self.bytes.load(Ordering::Relaxed))
    }

    /// A page's size as stored (compressed), if the store has it.
    pub fn size(&self, h: &Hash) -> Option<u64> {
        self.index.read().unwrap().get(h).map(|l| l.comp as u64)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.index.read().unwrap().len()
    }
}

/// Local packs whose number the remote (`listed`: names and sizes) has for a different pack: one
/// written here and never uploaded (a pack without its index here never was, and one whose size
/// or index isn't the remote's isn't the one uploaded), while another folder that took the lease over used
/// its number, and died before publishing so this folder could take it back. Paired with the
/// remote's index its pages would be misread, and `evict_local` would delete it as uploaded. Its
/// pages may still be needed (the log's `Op::Bulk` names trees in them), so it gets a number of
/// its own, after every number here and there, and is indexed from its own bytes; the remote's
/// pack of that number comes on demand, as any other.
fn set_aside_clashing(dir: &Path, listed: &HashMap<String, u64>, remote: &Remote) -> io::Result<()> {
    let num = |n: &str| n.strip_prefix("pack-")?.trim_end_matches(".idx").parse::<usize>().ok();
    let mut here: Vec<usize> =
        fs::read_dir(dir)?.filter_map(|e| num(&e.ok()?.file_name().into_string().ok()?)).collect();
    here.sort();
    here.dedup();
    let mut next = here.iter().copied().chain(listed.keys().filter_map(|n| num(n))).max().map_or(0, |n| n + 1);
    for id in here {
        let (name, path) = (pack_name(id), pack_path(dir, id));
        let Ok(meta) = fs::metadata(&path) else { continue };
        let theirs = listed.get(&name).copied();
        let clash = match idx_path(dir, id).exists() {
            false => theirs.is_some() || listed.contains_key(&format!("{name}.idx")),
            // same size isn't the same pack: the uploaded one's index is ours byte for byte
            // (an upload sends the index file as it is here)
            true => match theirs {
                None => false,
                Some(size) if size != meta.len() => true,
                Some(_) => {
                    let idx = format!("{name}.idx");
                    listed.contains_key(&idx) && remote.get(&idx)? != Some(fs::read(idx_path(dir, id))?)
                }
            },
        };
        if !clash {
            continue;
        }
        // the new index first, then the old one goes, then the pack moves: cut short anywhere,
        // the next open finds either this same clash or the pack moved (an index left without its
        // pack is dropped at open)
        let found: Vec<(Hash, Loc)> = scan(&path, next, true)?;
        disk::write_file(&idx_path(dir, next), &encode_idx(&found))?;
        match fs::remove_file(idx_path(dir, id)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => disk::sync_dir(dir)?,
        }
        fs::rename(&path, pack_path(dir, next))?;
        disk::sync_dir(dir)?;
        next += 1;
    }
    Ok(())
}

/// A record's stored bytes, at `l` in pack file `f`. Its lengths come from an index or a record
/// header, which aren't sealed with the page: one that runs past the end of the pack is damage,
/// said as such, not a size to allocate.
fn read_rec(f: &File, l: &Loc) -> io::Result<Vec<u8>> {
    if l.off.saturating_add(l.comp as u64) > f.metadata()?.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "is corrupt (its length runs past the end of its pack)",
        ));
    }
    let mut comp = vec![0; l.comp as usize];
    f.read_exact_at(&mut comp, l.off)?;
    Ok(comp)
}

/// A stored record's page: opened (if sealed), within the size a page this long could have,
/// decompressed, and named `h`; None if any of that fails.
fn decode_page(crypt: Option<&Crypt>, h: &Hash, raw: u32, comp: Vec<u8>) -> Option<Vec<u8>> {
    let comp = match crypt {
        Some(c) => c.open_page(h, &comp).ok()?,
        None => comp,
    };
    // the lengths aren't sealed with the page: a raw length no page this size could have (LZ4
    // expands at most about 255 times) is damage, not a size to allocate
    if raw as u64 > (comp.len() as u64 * 255 + 64).min(PAGE_MAX) {
        return None;
    }
    let bytes = lz4_flex::decompress(&comp, raw as usize).ok()?;
    let named = match crypt {
        Some(c) => c.name(&bytes),
        None => hash(&bytes),
    };
    (named == *h).then_some(bytes)
}

/// Reads the records of a pack that has no index file. A record cut short at the end of the
/// pack still being written is a crash mid-append and is truncated; anywhere else it's corruption.
fn scan(path: &Path, id: usize, last: bool) -> io::Result<Vec<(Hash, Loc)>> {
    let buf = fs::read(path)?;
    let (mut off, mut out) = (0, vec![]);
    while off < buf.len() {
        let rec = buf.get(off..off + HDR).map(|h| {
            let raw = u32::from_le_bytes(h[32..36].try_into().unwrap());
            let comp = u32::from_le_bytes(h[36..40].try_into().unwrap());
            (h[..32].try_into().unwrap(), raw, comp)
        });
        match rec {
            Some((h, raw, comp)) if off + HDR + comp as usize <= buf.len() => {
                out.push((h, Loc { pack: id, off: (off + HDR) as u64, raw, comp }));
                off += HDR + comp as usize;
            }
            _ if last => {
                let f = OpenOptions::new().write(true).open(path)?;
                f.set_len(off as u64)?; // torn append from a crash; never referenced
                f.sync_all()?;
                break;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is truncated at byte {off}", path.display()),
                ));
            }
        }
    }
    Ok(out)
}

pub fn hex(h: &Hash) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_gives_recently_used_pages_a_second_chance() {
        let mut c = Cache::new(300);
        let page = |n: u8| (([n; 32]), Arc::new(n) as Arc<dyn Any + Send + Sync>);
        for n in 1..=3 {
            let (h, p) = page(n);
            c.insert(h, p, 100);
        }
        c.map[&[1; 32]].used.store(true, Ordering::Relaxed); // page 1 was read
        let (h, p) = page(4);
        c.insert(h, p, 100);
        assert!(c.map.contains_key(&[1; 32]), "used page survives");
        assert!(!c.map.contains_key(&[2; 32]), "oldest unused page goes");
        assert!(c.map.contains_key(&[3; 32]) && c.map.contains_key(&[4; 32]));
        assert_eq!(c.bytes, 300);
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("chronos-pages-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// An append that fails part way (a full disk) must not shift where later records are read.
    #[test]
    fn a_failed_append_leaves_no_partial_record() {
        let dir = tmp("torn");
        let p = Pages::open(&dir, None, None).unwrap();
        let a = p.put(b"first page").unwrap();
        let pack = pack_path(&dir, p.w.lock().unwrap().id);
        {
            // what a full disk leaves: part of a record in the pack, then the write errors
            let mut w = p.w.lock().unwrap();
            OpenOptions::new().append(true).open(&pack).unwrap().write_all(&[0xab; 25]).unwrap();
            w.file = File::open(&pack).unwrap(); // read-only: this append fails
        }
        assert!(p.put(b"second page").is_err());
        p.w.lock().unwrap().file = OpenOptions::new().append(true).open(&pack).unwrap(); // space again
        let (b, c) = (p.put(b"third page").unwrap(), p.put(b"second page").unwrap());
        let want = [(a, &b"first page"[..]), (b, b"third page"), (c, b"second page")];
        for (h, bytes) in want {
            assert_eq!(p.read(&h).unwrap(), bytes);
        }
        drop(p);
        let p = Pages::open(&dir, None, None).unwrap();
        for (h, bytes) in want {
            assert_eq!(p.read(&h).unwrap(), bytes, "after reopening");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Cleanup cut short: an index whose pack is gone must not make its pages look stored (the
    /// same bytes stored again would be skipped, and a tree would name a missing page), and a
    /// finished pack whose index is gone is indexed again.
    /// A power cut can leave the pack being written with a record whose length is there but
    /// whose bytes are zeros: the page reads as corrupt (it isn't hidden), and storing the same
    /// page again writes it again, whole, rather than taking the damaged record as stored.
    #[test]
    fn a_zeroed_record_in_the_last_pack_is_written_again() {
        for key in [None, Some(Arc::new(Crypt::new(&[3; 32])))] {
            let dir = tmp("zeroed");
            let p = Pages::open(&dir, None, key.clone()).unwrap();
            let a = p.put(b"a synced page").unwrap();
            let b = p.put(b"a page the crash zeroed").unwrap();
            let loc = p.index.read().unwrap()[&b];
            let pack = pack_path(&dir, loc.pack);
            drop(p);
            let f = OpenOptions::new().write(true).open(&pack).unwrap();
            f.write_all_at(&vec![0; loc.comp as usize], loc.off).unwrap();
            let p = Pages::open(&dir, None, key).unwrap();
            assert!(p.read(&b).unwrap_err().to_string().contains("corrupt"));
            assert_eq!(p.put(b"a synced page").unwrap(), a); // whole: not written again
            assert_eq!(p.put(b"a page the crash zeroed").unwrap(), b);
            assert_eq!(p.read(&b).unwrap(), b"a page the crash zeroed");
            assert_eq!(p.read(&a).unwrap(), b"a synced page");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn indexes_and_packs_left_by_a_cleanup_cut_short() {
        let dir = tmp("orphans");
        let p = Pages::open(&dir, None, None).unwrap();
        let a = p.put(b"in a pack cleanup deleted").unwrap();
        p.seal(&mut p.w.lock().unwrap()).unwrap();
        let b = p.put(b"in the next pack").unwrap();
        p.seal(&mut p.w.lock().unwrap()).unwrap();
        let c = p.put(b"in the last pack").unwrap();
        drop(p);
        fs::remove_file(pack_path(&dir, 0)).unwrap(); // the pack went, its index stayed
        fs::remove_file(idx_path(&dir, 1)).unwrap(); // the index went, its pack stayed
        let p = Pages::open(&dir, None, None).unwrap();
        assert!(p.size(&a).is_none() && !idx_path(&dir, 0).exists(), "a missing pack's pages aren't stored");
        assert_eq!(p.put(b"in a pack cleanup deleted").unwrap(), a);
        assert_eq!(p.read(&a).unwrap(), b"in a pack cleanup deleted", "so storing them again writes them");
        assert!(idx_path(&dir, 1).exists(), "indexed again");
        assert_eq!(p.read(&b).unwrap(), b"in the next pack");
        assert_eq!(p.read(&c).unwrap(), b"in the last pack");
        drop(p);
        let p = Pages::open(&dir, None, None).unwrap();
        for (h, bytes) in [(a, &b"in a pack cleanup deleted"[..]), (b, b"in the next pack"), (c, b"in the last pack")] {
            assert_eq!(p.read(&h).unwrap(), bytes);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Record lengths aren't sealed with the page: tampered ones are a "corrupt" error, never an
    /// allocation of whatever they claim.
    #[test]
    fn tampered_lengths_are_corrupt_not_a_crash() {
        let dir = tmp("lengths");
        let p = Pages::open(&dir, None, Some(Arc::new(Crypt::new(&[7; 32])))).unwrap();
        let h = p.put(b"a page with a length").unwrap();
        let good = p.index.read().unwrap()[&h];
        for bad in [Loc { comp: u32::MAX, ..good }, Loc { raw: u32::MAX, ..good }, Loc { raw: good.comp * 300, ..good }]
        {
            p.index.write().unwrap().insert(h, bad);
            let e = p.read(&h).unwrap_err();
            assert!(e.to_string().contains("corrupt"), "{e}");
        }
        p.index.write().unwrap().insert(h, good);
        assert_eq!(p.read(&h).unwrap(), b"a page with a length");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A double takeover: A dies with pages in an unfinished pack N, B takes over, seals and uploads
    /// its own pack N and dies before publishing, and A takes over again. A's pack must not be
    /// read through B's index (nor deleted as uploaded): it moves to a number of its own.
    #[cfg(feature = "s3")]
    #[test]
    fn a_local_pack_never_pairs_with_another_folders_index() {
        let (a, b, bucket) = (tmp("clash-a"), tmp("clash-b"), tmp("clash-bucket"));
        let url = format!("file://{}", bucket.display());
        let open = |dir: &Path, owner: &str| {
            let r = Remote::new(&url).unwrap();
            remote::acquire(&r, owner, true).unwrap();
            Pages::open(dir, Some((r, owner.to_string())), None).unwrap()
        };
        let p = open(&a, "a");
        let first = p.put(b"published by a").unwrap();
        p.sync().unwrap(); // pack 0, uploaded
        let mine = p.put(b"only in a's unfinished pack").unwrap();
        drop(p); // dies
        let p = open(&b, "b");
        let theirs = p.put(b"in b's pack of the same number").unwrap();
        p.sync().unwrap(); // b's pack 1 and its index, uploaded
        drop(p); // dies before publishing
        let p = open(&a, "a");
        assert_eq!(p.read(&mine).unwrap(), b"only in a's unfinished pack");
        assert_eq!(p.read(&theirs).unwrap(), b"in b's pack of the same number");
        assert_eq!(p.read(&first).unwrap(), b"published by a");
        p.sync().unwrap();
        p.evict_local().unwrap();
        assert_eq!(p.read(&mine).unwrap(), b"only in a's unfinished pack", "uploaded before it was evicted");
        drop(p);
        for d in [a, b, bucket] {
            fs::remove_dir_all(d).unwrap();
        }
    }
}
