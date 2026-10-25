//! WAL shipping (docs/designs/wal-shipping.md): with a remote, the log goes to it as it is
//! written, in chunks, so another machine opening the remote (a takeover after this one died, or
//! a database moved to another host) has every commit that was shipped, not only the last
//! checkpoint.
//!
//!   wal-<segment, 16 digits>-<offset, 16 digits>   one chunk: a log segment's bytes from that
//!                                                    offset, exactly as on disk (framed, encrypted)
//!
//! The chunks from the manifest's first segment on form a chain: each starts where the one before
//! ended, or at the next segment's start after a sealed one (the segment ends there). So the next
//! chunk's name is always known, and an object created only if absent is the fence: a takeover
//! puts a fence chunk at the chain's end, after which the old writer's next upload, which can only
//! have that name, finds it taken and stops. One upload is in flight at a time, so the chain has
//! no gaps.
//!
//! A checkpoint ships its cut segment, sealed, before it writes the manifest that starts after it,
//! then deletes the chunks the manifest before it needed (one checkpoint of slack, for a reader
//! tailing the chain).

use crate::disk::{self, Log};
use crate::remote::Remote;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
// std's, not `crate::sync`'s: only a database with a remote ships, which the simulation tests don't open
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const MAGIC: &[u8] = b"SONOSWAL1";
/// A chunk holds at most this much log, unless one record alone is bigger.
pub(crate) const CHUNK_BYTES: u64 = 4 << 20;
const SEALED: u8 = 1;
const FENCE: u8 = 2;
/// In `async` mode, writers wait for the shipper once this much log is unshipped, or the oldest
/// unshipped byte is this old: the window the mode promises stays bounded while the remote is slow.
const LAG_BYTES: u64 = 64 << 20;
const LAG: Duration = Duration::from_secs(30);
/// In `sync` mode, new writes are refused once uploads have failed this long (commits under way
/// keep waiting): clients get an answer instead of piling up behind the remote.
const REFUSE_AFTER: Duration = Duration::from_secs(10);
/// A checkpoint waiting for its segment to be shipped gives up after uploads failed this long.
const SEAL_PATIENCE: Duration = Duration::from_secs(30);

/// How commits wait for the remote (`wal_shipping` in the folder; `async` if unset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shipping {
    /// The log stays on local disk until a checkpoint, as without shipping.
    Off = 0,
    /// Shipped within `delay` of being written; commits don't wait for it.
    Async = 1,
    /// A commit returns once its log is in the remote.
    Sync = 2,
}

impl Shipping {
    fn from_u64(v: u64) -> Shipping {
        match v {
            0 => Shipping::Off,
            2 => Shipping::Sync,
            _ => Shipping::Async,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Shipping::Off => "off",
            Shipping::Async => "async",
            Shipping::Sync => "sync",
        }
    }

    /// `off`, `async` or `sync`/`on` (any case).
    pub fn parse(s: &str) -> Option<Shipping> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Shipping::Off),
            "async" => Some(Shipping::Async),
            "sync" | "on" => Some(Shipping::Sync),
            _ => None,
        }
    }
}

/// The folder's setting: `<mode> <delay ms>`. CHRONOS_WAL_SHIPPING and
/// CHRONOS_WAL_SHIPPING_DELAY_MS, when set, win (an operator's choice for every database it runs).
pub(crate) fn setting(dir: &Path) -> io::Result<(Shipping, u64)> {
    let saved = fs::read_to_string(dir.join("wal_shipping")).unwrap_or_default();
    let mut words = saved.split_whitespace();
    let mut mode = words.next().and_then(Shipping::parse).unwrap_or(Shipping::Async);
    let mut delay = words.next().and_then(|d| d.parse().ok()).unwrap_or(1000);
    if let Ok(v) = std::env::var("CHRONOS_WAL_SHIPPING") {
        mode = Shipping::parse(&v).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("CHRONOS_WAL_SHIPPING={v}: use off, async or sync"))
        })?;
    }
    if let Ok(v) = std::env::var("CHRONOS_WAL_SHIPPING_DELAY_MS") {
        delay = v.trim().parse().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("CHRONOS_WAL_SHIPPING_DELAY_MS={v}: a number of ms"))
        })?;
    }
    Ok((mode, delay))
}

pub(crate) fn save_setting(dir: &Path, mode: Shipping, delay_ms: u64) -> io::Result<()> {
    disk::write_file(&dir.join("wal_shipping"), format!("{} {delay_ms}", mode.name()).as_bytes())
}

/// The error of a chain that changed while it was read: a checkpoint published a newer manifest
/// and deleted chunks the older one needed (see `reconcile`).
#[derive(Debug)]
struct MovedOn(String);

impl std::fmt::Display for MovedOn {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MovedOn {}

fn moved_on(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|e| e.is::<MovedOn>())
}

pub(crate) fn name(seg: u64, off: u64) -> String {
    format!("wal-{seg:016}-{off:016}")
}

fn parse_name(n: &str) -> Option<(u64, u64)> {
    let (s, o) = n.strip_prefix("wal-")?.split_once('-')?;
    (s.len() == 16 && o.len() == 16).then_some(())?;
    Some((s.parse().ok()?, o.parse().ok()?))
}

/// One uploaded byte range of a log segment.
pub(crate) struct Chunk {
    pub seg: u64,
    pub off: u64,
    flags: u8,
    /// The writer that uploaded it (`remote::owner`).
    pub owner: String,
    pub body: Vec<u8>,
}

impl Chunk {
    fn sealed(&self) -> bool {
        self.flags & SEALED != 0
    }

    fn fence(&self) -> bool {
        self.flags & FENCE != 0
    }

    /// Where the next chunk starts.
    fn next(&self) -> (u64, u64) {
        match self.sealed() {
            true => (self.seg + 1, 0),
            false => (self.seg, self.off + self.body.len() as u64),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = MAGIC.to_vec();
        b.extend(self.seg.to_le_bytes());
        b.extend(self.off.to_le_bytes());
        b.extend((self.body.len() as u32).to_le_bytes());
        b.push(self.flags);
        b.extend((self.owner.len() as u32).to_le_bytes());
        b.extend(self.owner.as_bytes());
        b.extend(crc32fast::hash(&self.body).to_le_bytes());
        b.extend(&self.body);
        b
    }

    fn decode(name: &str, b: &[u8]) -> io::Result<Chunk> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("log chunk {name} in the remote is damaged"));
        let mut r = b.strip_prefix(MAGIC).ok_or_else(bad)?;
        let mut take = |n: usize| -> io::Result<&[u8]> {
            let (a, rest) = r.split_at_checked(n).ok_or_else(bad)?;
            r = rest;
            Ok(a)
        };
        let u64_ = |b: &[u8]| u64::from_le_bytes(b.try_into().unwrap());
        let u32_ = |b: &[u8]| u32::from_le_bytes(b.try_into().unwrap());
        let (seg, off, len, flags) = (u64_(take(8)?), u64_(take(8)?), u32_(take(4)?) as usize, take(1)?[0]);
        let owner_len = u32_(take(4)?) as usize;
        let owner = String::from_utf8(take(owner_len)?.to_vec()).map_err(|_| bad())?;
        let crc = u32_(take(4)?);
        let body = take(len)?.to_vec();
        if !r.is_empty() || crc32fast::hash(&body) != crc || parse_name(name) != Some((seg, off)) {
            return Err(bad());
        }
        Ok(Chunk { seg, off, flags, owner, body })
    }
}

/// The chain from segment `first` on, every chunk in order. A chunk past a gap is refused, as a
/// gap in the local log is: data in it would be lost silently.
pub(crate) fn chain(r: &Remote, first: u64) -> io::Result<Vec<Chunk>> {
    let mut names: Vec<(u64, u64)> =
        r.list_after(&format!("wal-{first:016}"))?.iter().filter_map(|(n, _)| parse_name(n)).collect();
    names.retain(|&(s, _)| s >= first);
    names.sort();
    let wanted: Vec<String> = names.iter().map(|&(s, o)| name(s, o)).collect();
    let mut got = std::collections::BTreeMap::new();
    for (n, b) in wanted.iter().zip(r.get_many(&wanted)?) {
        let b = b.ok_or_else(|| {
            io::Error::other(MovedOn(format!(
                "log chunk {n} went away while it was read (a checkpoint moved on); open again"
            )))
        })?;
        let c = Chunk::decode(n, &b)?;
        got.insert((c.seg, c.off), c);
    }
    let mut out = vec![];
    let mut at = (first, 0);
    while let Some(c) = got.remove(&at) {
        at = c.next();
        out.push(c);
    }
    if let Some(&(s, o)) = got.keys().next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: the log in the remote has a gap at {} and goes on after it ({}); refusing to guess",
                r.url(),
                name(at.0, at.1),
                name(s, o)
            ),
        ));
    }
    Ok(out)
}

fn end(chain: &[Chunk], first: u64) -> (u64, u64) {
    chain.last().map_or((first, 0), Chunk::next)
}

/// After taking the lease from another writer: puts a fence at the chain's end, so that writer
/// can upload nothing more. If its last chunk lands first, the fence goes after that.
fn fence(r: &Remote, first: u64, owner: &str) -> io::Result<()> {
    let mut chain = chain(r, first)?;
    loop {
        let (seg, off) = end(&chain, first);
        let f = Chunk { seg, off, flags: SEALED | FENCE, owner: owner.into(), body: vec![] };
        if r.put_new(&name(seg, off), f.encode())? {
            return Ok(());
        }
        let new = self::chain(r, first)?;
        if let Some(c) = new.iter().find(|c| (c.seg, c.off) == (seg, off))
            && c.fence()
            && c.owner != owner
        {
            return Err(io::Error::other(format!("{} was taken over by {} meanwhile", r.url(), c.owner)));
        }
        chain = new;
    }
}

/// Lines this folder's log up with the remote's chain, before the log is replayed (after
/// `remote::adopt`, with the lease held). Where the chain has bytes, they win: the folder's own
/// differ only if the machine lost them unsynced after they were shipped. Past the chain's end the folder's log is kept, still to be shipped, if the chain
/// ends with this folder's own chunk; if it ends with another writer's, the folder fell behind:
/// refused, or with `takeover` set aside. After a takeover (`lease`), a fence comes first, and
/// nothing this folder had past the old writer's last chunk is kept.
/// Returns where shipping goes on from, the manifest's first segment, and whether it set the
/// folder's database aside.
pub(crate) fn reconcile(
    r: &Remote,
    dir: &Path,
    c: Option<&crate::crypt::Crypt>,
    owner: &str,
    lease: crate::remote::Lease,
    takeover: bool,
) -> io::Result<Reconciled> {
    let mut tries = 0;
    loop {
        match reconcile_once(r, dir, c, owner, lease, takeover) {
            // the writer it took over from (or a stale one) checkpointed meanwhile: its manifest
            // holds what the deleted chunks did, so start again from it
            Err(e) if moved_on(&e) && tries < 5 => {
                tries += 1;
                let theirs =
                    r.get("manifest")?.ok_or_else(|| io::Error::other(format!("{} has no manifest", r.url())))?;
                disk::write_file(&dir.join("manifest"), &theirs)?;
                crate::remote::saw(dir, &theirs)?;
            }
            r => return r,
        }
    }
}

fn reconcile_once(
    r: &Remote,
    dir: &Path,
    c: Option<&crate::crypt::Crypt>,
    owner: &str,
    lease: crate::remote::Lease,
    takeover: bool,
) -> io::Result<Reconciled> {
    let first = disk::manifest_first(dir, c)?;
    let took_over = lease == crate::remote::Lease::TakenOver;
    if took_over {
        fence(r, first, owner)?;
    }
    let chain = chain(r, first)?;
    if chain.is_empty() {
        // nothing shipped since the manifest (as after a clean close): the folder's log, if any, goes up
        return Ok(Reconciled { at: (first, 0), first, set_aside: false });
    }
    let (end_seg, end_off) = end(&chain, first);
    // the chain's bytes per segment, from its start
    let mut want: std::collections::BTreeMap<u64, Vec<u8>> = std::collections::BTreeMap::new();
    let mut sealed = std::collections::BTreeSet::new();
    for c in &chain {
        want.entry(c.seg).or_default().extend(&c.body);
        if c.sealed() {
            sealed.insert(c.seg);
        }
    }
    let ours = !took_over && chain.last().is_none_or(|c| c.owner == owner);
    let local: Vec<u64> = disk::segments(dir)?.into_iter().filter(|&s| s >= first).collect();
    let read = |s: u64| fs::read(disk::seg_path(dir, s));
    // does the folder hold anything the chain contradicts, or that comes after another writer's end?
    let mut differs = false;
    let mut ahead = false;
    for &s in &local {
        let have = read(s)?;
        match want.get(&s) {
            Some(w) => {
                let n = have.len().min(w.len());
                differs |= have[..n] != w[..n] || have.len() > w.len() && sealed.contains(&s);
                ahead |= have.len() > w.len();
            }
            None => ahead |= s < end_seg || !have.is_empty(),
        }
    }
    if !ours && (differs || ahead) && !took_over {
        if !takeover {
            return Err(io::Error::other(format!(
                "{} has log this folder hasn't seen (another folder wrote to it since); open it from a new, empty \
                 folder, or with takeover to set this folder's database aside",
                r.url()
            )));
        }
        let theirs = r.get("manifest")?.ok_or_else(|| io::Error::other(format!("{} has no manifest", r.url())))?;
        crate::remote::set_aside_database(dir)?;
        crate::remote::take_settings(r, dir)?;
        disk::write_file(&dir.join("manifest"), &theirs)?;
        crate::remote::saw(dir, &theirs)?;
        let again = reconcile(r, dir, c, owner, lease, false)?; // a folder with no log of its own now
        return Ok(Reconciled { set_aside: true, ..again });
    } else if differs || took_over && ahead {
        // the chain wins; what it overwrites is kept aside, not dropped
        let aside = dir.join(format!("stale-log-{}", disk::now_ms()));
        fs::create_dir(&aside)?;
        for &s in &local {
            fs::copy(disk::seg_path(dir, s), aside.join(format!("wal-{s:016}")))?;
        }
        disk::sync_dir(&aside)?;
    }
    let keep_extra = ours;
    for (&s, w) in &want {
        let path = disk::seg_path(dir, s);
        let have = read(s).unwrap_or_default();
        let mut new = w.clone();
        if keep_extra && have.len() > w.len() && !sealed.contains(&s) {
            new.extend(&have[w.len()..]);
        }
        if new != have {
            let tmp = path.with_extension("download");
            let mut f = File::create(&tmp)?;
            io::Write::write_all(&mut f, &new)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)?;
        }
    }
    if !keep_extra {
        // after the chain's end: another writer's chain ends there, or a fence just did
        for &s in local.iter().filter(|&&s| !want.contains_key(&s)) {
            fs::remove_file(disk::seg_path(dir, s))?;
        }
    }
    if chain.last().is_some_and(Chunk::sealed) && !disk::seg_path(dir, end_seg).exists() {
        File::create(disk::seg_path(dir, end_seg))?; // the log goes on in the next segment
    }
    disk::sync_dir(dir)?;
    Ok(Reconciled { at: (end_seg, end_off), first, set_aside: false })
}

pub(crate) struct Reconciled {
    /// Where shipping goes on from: the chain's end.
    pub at: (u64, u64),
    /// The first segment of the manifest the folder opens with.
    pub first: u64,
    /// The folder's own database was set aside (it fell behind the remote).
    pub set_aside: bool,
}

/// The shipper's state, shared with the commits and checkpoints that wait for it.
pub(crate) struct Ship {
    remote: Remote,
    owner: String,
    mode: AtomicU64,
    delay_ms: AtomicU64,
    st: Mutex<State>,
    cv: Condvar,
    /// Chunks uploaded, and uploads that failed, since the database opened (for `/metrics`).
    uploads: AtomicU64,
    failures: AtomicU64,
}

struct State {
    /// Shipped through here: (segment, offset).
    at: (u64, u64),
    /// Every record the log appended up to this position (`Log::appended`) is shipped.
    shipped: u64,
    /// Commits are waiting for log up to here.
    want: u64,
    /// A checkpoint waits for segments up to this one to be shipped, sealed.
    seal: Option<u64>,
    /// When unshipped log was first seen (async: shipped `delay` after).
    since: Option<Instant>,
    /// Another writer took the remote over: what to say.
    fenced: Option<String>,
    /// Uploads have failed since then, the last with this error.
    failing: Option<(Instant, String)>,
    /// The first segment of the manifest before the last one published: chunks before it go next.
    prev_first: u64,
    /// The shipper thread runs: until then (while the database opens) nothing waits for it.
    running: bool,
    stop: bool,
}

impl Ship {
    /// Shipping from `at` (from `reconcile`) for `owner`, through its own connection to `url`.
    pub(crate) fn new(url: &str, owner: &str, dir: &Path, at: (u64, u64), first: u64) -> io::Result<Ship> {
        let (mode, delay) = setting(dir)?;
        Ok(Ship {
            remote: Remote::new(url)?,
            owner: owner.into(),
            mode: AtomicU64::new(mode as u64),
            delay_ms: AtomicU64::new(delay),
            st: Mutex::new(State {
                at,
                shipped: 0,
                want: 0,
                seal: None,
                since: None,
                fenced: None,
                failing: None,
                prev_first: first,
                running: false,
                stop: false,
            }),
            cv: Condvar::new(),
            uploads: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        })
    }

    pub(crate) fn mode(&self) -> Shipping {
        Shipping::from_u64(self.mode.load(Ordering::Relaxed))
    }

    pub(crate) fn delay_ms(&self) -> u64 {
        self.delay_ms.load(Ordering::Relaxed)
    }

    pub(crate) fn set(&self, dir: &Path, mode: Shipping, delay_ms: u64) -> io::Result<()> {
        save_setting(dir, mode, delay_ms)?;
        self.mode.store(mode as u64, Ordering::Relaxed);
        self.delay_ms.store(delay_ms, Ordering::Relaxed);
        self.cv.notify_all();
        Ok(())
    }

    /// Log appended but not yet shipped, in bytes.
    pub(crate) fn lag(&self, log: &Log) -> u64 {
        log.appended().saturating_sub(self.st.lock().unwrap().shipped)
    }

    /// For `/metrics`: unshipped bytes, how long the oldest has waited (ms), chunks uploaded,
    /// uploads failed, and 1 if another writer took the remote over.
    pub(crate) fn counters(&self, log: &Log) -> [(&'static str, u64); 5] {
        let st = self.st.lock().unwrap();
        let waited = st.since.filter(|_| log.appended() > st.shipped).map_or(0, |t| t.elapsed().as_millis() as u64);
        [
            ("wal_shipping_lag_bytes", log.appended().saturating_sub(st.shipped)),
            ("wal_shipping_lag_ms", waited),
            ("wal_shipping_uploads", self.uploads.load(Ordering::Relaxed)),
            ("wal_shipping_upload_failures", self.failures.load(Ordering::Relaxed)),
            ("wal_shipping_fenced", u64::from(st.fenced.is_some())),
        ]
    }

    /// Why a new write must be refused now, if it must: in `sync` mode, a commit can't reach the
    /// remote (uploads have failed for a while, or another writer took it over).
    pub(crate) fn refusing(&self) -> Option<String> {
        if self.mode() != Shipping::Sync {
            return None;
        }
        let st = self.st.lock().unwrap();
        if let Some(why) = &st.fenced {
            return Some(format!("{why}; this server writes nothing more"));
        }
        let (since, e) = st.failing.as_ref()?;
        (since.elapsed() >= REFUSE_AFTER).then(|| {
            format!(
                "object storage unreachable for {} s ({e}); writes are paused until the log can be shipped, reads work",
                since.elapsed().as_secs()
            )
        })
    }

    /// For a commit: in `sync` mode, waits until the log up to `upto` is in the remote. A writer
    /// taken over meanwhile can't say its commit is safe, and has nothing else to say: it stops,
    /// as when an fsync fails.
    pub(crate) fn wait(&self, log: &Log, upto: u64) {
        match self.mode() {
            Shipping::Sync => {}
            // a promise of a window: writers that get far ahead of it wait for it
            Shipping::Async
                if {
                    let st = self.st.lock().unwrap();
                    log.appended().saturating_sub(st.shipped) > LAG_BYTES || st.since.is_some_and(|t| t.elapsed() > LAG)
                } => {}
            _ => return,
        }
        let mut st = self.st.lock().unwrap();
        st.want = st.want.max(upto);
        self.cv.notify_all();
        while st.shipped < upto && st.running && !st.stop {
            if let Some(why) = &st.fenced {
                disk::fatal(io::Error::other(why.clone()));
            }
            st = self.cv.wait(st).unwrap();
        }
    }

    /// For a checkpoint: waits until segments up to `seg` are shipped, sealed. Fails if this writer
    /// was taken over, or uploads have failed for a while (the checkpoint is tried again later).
    pub(crate) fn wait_sealed(&self, seg: u64) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.seal = Some(st.seal.map_or(seg, |s| s.max(seg)));
        self.cv.notify_all();
        // while the database opens, the shipper seals it once it starts (before anything ships after)
        while st.at.0 <= seg && st.running {
            if let Some(why) = &st.fenced {
                return Err(io::Error::other(why.clone()));
            }
            if let Some((t, e)) = &st.failing
                && t.elapsed() > SEAL_PATIENCE
            {
                return Err(io::Error::other(format!("shipping the log to {} failed: {e}", self.remote.url())));
            }
            if st.stop {
                return Err(io::Error::other("closing"));
            }
            st = self.cv.wait_timeout(st, Duration::from_secs(1)).unwrap().0;
        }
        Ok(())
    }

    /// After a checkpoint published a manifest starting at segment `first`: deletes the chunks
    /// only the manifest before the last one needed.
    pub(crate) fn collect(&self, r: &Remote, first: u64) -> io::Result<()> {
        let before = std::mem::replace(&mut self.st.lock().unwrap().prev_first, first);
        for (n, _) in r.list_after("wal-")? {
            if parse_name(&n).is_some_and(|(s, _)| s < before) {
                r.delete(&n)?;
            }
        }
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.st.lock().unwrap().stop = true;
        self.cv.notify_all();
    }
}

/// The shipper thread: uploads the log of `log` until `Ship::stop`.
pub(crate) fn run(log: &Log, ship: &Ship) {
    ship.st.lock().unwrap().running = true;
    let mut backoff = Duration::from_millis(100);
    loop {
        {
            let mut st = ship.st.lock().unwrap();
            loop {
                if st.stop {
                    return;
                }
                let pending = log.appended() > st.shipped;
                let wanted = st.want > st.shipped || st.seal.is_some_and(|s| st.at.0 <= s);
                let mut wait = Duration::from_millis(100); // appends don't wake this thread
                // `off` set while open: shipped only as checkpoints need it, so the chain stays whole
                if st.fenced.is_none() && (wanted || pending && ship.mode() != Shipping::Off) {
                    let since = *st.since.get_or_insert_with(Instant::now);
                    let delay = Duration::from_millis(ship.delay_ms());
                    if wanted || log.appended() - st.shipped >= CHUNK_BYTES || since.elapsed() >= delay {
                        break;
                    }
                    wait = wait.min(delay - since.elapsed());
                }
                st = ship.cv.wait_timeout(st, wait).unwrap().0;
            }
        }
        match ship_once(log, ship) {
            Ok(()) => {
                backoff = Duration::from_millis(100);
                ship.st.lock().unwrap().failing = None;
            }
            Err(Failed::Fenced(why)) => {
                crate::obs::log_line("ERROR", "wal_ship_stopped", &[("detail", why.to_string())]);
                ship.st.lock().unwrap().fenced = Some(why);
                ship.cv.notify_all();
            }
            Err(Failed::Io(e)) => {
                ship.failures.fetch_add(1, Ordering::Relaxed);
                let mut st = ship.st.lock().unwrap();
                if st.failing.is_none() {
                    crate::obs::log_line(
                        "WARN",
                        "wal_ship_failed",
                        &[
                            ("remote", ship.remote.url().to_string()),
                            ("error", format!("{e}")),
                            ("detail", "retrying".into()),
                        ],
                    );
                }
                let since = st.failing.take().map_or_else(Instant::now, |(t, _)| t);
                st.failing = Some((since, e.to_string()));
                ship.cv.notify_all(); // checkpoints waiting may give up
                drop(ship.cv.wait_timeout(st, backoff).unwrap());
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

enum Failed {
    Fenced(String),
    Io(io::Error),
}

impl From<io::Error> for Failed {
    fn from(e: io::Error) -> Failed {
        Failed::Io(e)
    }
}

/// Ships everything written so far: what is left of earlier segments, sealed, then the live one.
fn ship_once(log: &Log, ship: &Ship) -> Result<(), Failed> {
    let (seg, off, upto) = log.cut_end()?;
    let mut at = ship.st.lock().unwrap().at;
    while at.0 < seg {
        let len = fs::metadata(disk::seg_path(&log.dir, at.0))?.len();
        upload(log, ship, at, len, true)?;
        at = (at.0 + 1, 0);
    }
    if off > at.1 {
        upload(log, ship, at, off, false)?;
    }
    let mut st = ship.st.lock().unwrap();
    st.shipped = st.shipped.max(upto);
    st.since = None;
    ship.cv.notify_all();
    Ok(())
}

/// Uploads segment `at.0` from `at.1` to `to` (whole records), in chunks of up to `CHUNK_BYTES`;
/// `sealed`: the segment ends at `to`.
fn upload(log: &Log, ship: &Ship, at: (u64, u64), to: u64, sealed: bool) -> Result<(), Failed> {
    use std::os::unix::fs::FileExt;
    let (seg, from) = at;
    let mut bytes = vec![0; (to - from) as usize];
    if to > from {
        OpenOptions::new().read(true).open(disk::seg_path(&log.dir, seg))?.read_exact_at(&mut bytes, from)?;
    }
    // cut at record boundaries: [u32 length][u32 crc][payload]
    let mut cuts = vec![];
    let (mut p, mut start) = (0usize, 0usize);
    while p < bytes.len() {
        let len = u32::from_le_bytes(bytes.get(p..p + 4).ok_or_else(torn)?.try_into().unwrap()) as usize + 8;
        if p > start && (p + len - start) as u64 > CHUNK_BYTES {
            cuts.push(start..p);
            start = p;
        }
        p += len;
    }
    if p != bytes.len() {
        return Err(torn().into());
    }
    cuts.push(start..p);
    let last = cuts.len() - 1;
    for (i, r) in cuts.into_iter().enumerate() {
        let c = Chunk {
            seg,
            off: from + r.start as u64,
            flags: if sealed && i == last { SEALED } else { 0 },
            owner: ship.owner.clone(),
            body: bytes[r].to_vec(),
        };
        let n = name(c.seg, c.off);
        let encoded = c.encode();
        if !ship.remote.put_new(&n, encoded.clone())? {
            // ours already, if an earlier try landed but its answer was lost
            match ship.remote.get(&n)? {
                Some(b) if b == encoded => {}
                Some(b) => {
                    let who = Chunk::decode(&n, &b).map_or_else(|_| "another writer".into(), |c| c.owner);
                    return Err(Failed::Fenced(format!("{} was taken over by {who}", ship.remote.url())));
                }
                None => return Err(io::Error::other(format!("{n} was there, then wasn't")).into()),
            }
        }
        ship.uploads.fetch_add(1, Ordering::Relaxed);
        let mut st = ship.st.lock().unwrap();
        st.at = c.next();
        ship.cv.notify_all();
    }
    Ok(())
}

fn torn() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "a log segment ends inside a record; not shipping past it")
}

/// Starts the shipper thread for `log`, if it ships.
pub(crate) fn start(core: Arc<crate::Core>) -> Option<std::thread::JoinHandle<()>> {
    core.log.as_ref()?.ship.as_ref()?;
    Some(std::thread::spawn(move || {
        let log = core.log.as_ref().unwrap();
        run(log, log.ship.as_ref().unwrap());
    }))
}
