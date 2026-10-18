//! Spilling to disk: queries whose working state outgrows `CHRONOS_WORK_MEM` (default 256 MB)
//! write rows to temporary files and work through them a part at a time. Files go in
//! `CHRONOS_SPILL_DIR` (default the system's temporary folder): made new (never a file or link
//! already there), readable by this user only, sealed with the database's key if it has one, and
//! unlinked as soon as they're open, so nothing is left behind even by a crash.

use super::{Batch, Dir, Value, order_cmp, order_positions, pack, unpack};
use crate::crypt::Crypt;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bytes a query may hold in one grouping or sort before it spills: CHRONOS_WORK_MEM, or for an
/// agent with max_memory_mb, a quarter of that if less (a query holds more than one such part).
pub(crate) fn work_mem() -> usize {
    static MEM: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let all =
        *MEM.get_or_init(|| std::env::var("CHRONOS_WORK_MEM").ok().and_then(|s| parse_size(&s)).unwrap_or(256 << 20));
    super::mem_limit().map_or(all, |m| all.min(m.cap / 4))
}

/// `64MB`, `2GB`, `512kB` or plain bytes.
fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: usize = n.parse().ok()?;
    let times = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" | "k" => 1 << 10,
        "mb" | "m" => 1 << 20,
        "gb" | "g" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(times).filter(|n| *n > 0)
}

/// Bytes spilled since the process started (a metric).
pub(crate) static SPILLED: AtomicU64 = AtomicU64::new(0);

/// Bytes in the spill files open now, across the process: with CHRONOS_SPILL_DIR inside a
/// database's folder, they count toward its cap (see `Core::disk_bytes`).
pub(crate) static HELD: AtomicU64 = AtomicU64::new(0);

/// About how much memory values take.
pub(crate) fn size_of(vals: &[Value]) -> usize {
    vals.iter()
        .map(|v| {
            32 + match v {
                Value::Text(t) | Value::JsonText(t) => t.len(),
                Value::Json(j) => 64 + j.to_string().len() / 2,
                Value::Vector(x) => x.len() * 4,
                Value::Bytea(b) => b.len() * 2,
                Value::Net(_) => 16,
                _ => 0,
            }
        })
        .sum()
}

/// A temporary file of rows, each with a sequence number (its place in the table), written in
/// blocks (each sealed, if the database has a key).
pub(crate) struct Spill {
    file: File,     // unlinked: gone once closed
    block: Vec<u8>, // rows not yet written: seq, length, packed row
    read: bool,     // no more writes
    crypt: Option<Arc<Crypt>>,
    width: usize,
    left: Option<PathBuf>, // its name, if unlinking it failed: tried again when it's dropped
    held: u64,             // bytes written, in `HELD` until it's dropped
}

/// A new file in `dir` named `<prefix>-<pid>-<random>`: made new (never a file or link already
/// there) and readable by this user only.
pub(crate) fn create(dir: &std::path::Path, prefix: &str) -> io::Result<(File, PathBuf)> {
    use ring::rand::SecureRandom;
    use std::os::unix::fs::OpenOptionsExt;
    let mut r = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut r).map_err(|_| io::Error::other("no random numbers from the system"))?;
    let name: String = r.iter().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("{prefix}-{}-{name}", std::process::id()));
    let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
    Ok((file, path))
}

impl Drop for Spill {
    fn drop(&mut self) {
        HELD.fetch_sub(self.held, Ordering::Relaxed);
        if let Some(p) = &self.left {
            let _ = std::fs::remove_file(p);
        }
    }
}

const BLOCK: usize = 1 << 16;
const AAD: &[u8] = b"sonos spill rows";

impl Spill {
    pub fn new(width: usize) -> io::Result<Spill> {
        let dir = std::env::var_os("CHRONOS_SPILL_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        std::fs::create_dir_all(&dir)?; // inside the database's folder, opening it emptied that folder
        let (file, path) = create(&dir, "sonos-spill")?;
        // unlinked now, so a crash leaves nothing; if that fails, when it's dropped
        let left = std::fs::remove_file(&path).is_err().then_some(path);
        // the running statement's database: its key, if it has one
        let crypt = super::STOP.with_borrow(|s| s.as_ref()?.ctx.as_ref()?.core.spill_dir()?.1);
        Ok(Spill { file, block: vec![], read: false, crypt, width, left, held: 0 })
    }

    pub fn write(&mut self, seq: u64, row: &[Value]) -> io::Result<()> {
        assert!(!self.read, "written before read");
        let body = pack(row, 0);
        let b = &mut self.block;
        b.extend(seq.to_le_bytes());
        b.extend((body.len() as u32).to_le_bytes());
        b.extend(body.as_bytes());
        SPILLED.fetch_add(12 + body.len() as u64, Ordering::Relaxed);
        if b.len() >= BLOCK {
            self.put()?;
        }
        Ok(())
    }

    /// Writes the rows held: their length, then them (sealed).
    fn put(&mut self) -> io::Result<()> {
        let sealed = self.crypt.as_ref().map(|c| c.seal(AAD, &self.block));
        let data = sealed.as_deref().unwrap_or(&self.block);
        let n = 4 + data.len() as u64;
        // the running statement's database's cap, when its spill files count toward it
        let room = super::STOP.with_borrow(|s| Some(s.as_ref()?.ctx.as_ref()?.core.spill_room(n)));
        if let Some(Err(e)) = room {
            return Err(io::Error::new(io::ErrorKind::StorageFull, e.to_string()));
        }
        let mut w = &self.file;
        w.write_all(&(data.len() as u32).to_le_bytes())?;
        w.write_all(data)?;
        HELD.fetch_add(n, Ordering::Relaxed);
        self.held += n;
        self.block.clear();
        Ok(())
    }

    /// Its rows, in the order they were written (again each time it's called).
    pub fn read(&mut self) -> io::Result<impl Iterator<Item = io::Result<(u64, Vec<Value>)>> + '_> {
        if !self.read && !self.block.is_empty() {
            self.put()?;
        }
        self.read = true;
        let mut r = BufReader::with_capacity(1 << 16, At(&self.file, 0));
        let (width, crypt) = (self.width, self.crypt.clone());
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "a spill file is damaged");
        let (mut block, mut at) = (vec![], 0);
        Ok(std::iter::from_fn(move || {
            if at == block.len() {
                let mut len = [0u8; 4];
                match r.read_exact(&mut len) {
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return None,
                    Err(e) => return Some(Err(e)),
                    Ok(()) => {}
                }
                let mut data = vec![0u8; u32::from_le_bytes(len) as usize];
                if let Err(e) = r.read_exact(&mut data) {
                    return Some(Err(e));
                }
                block = match &crypt {
                    Some(c) => match c.open(AAD, &data, "a spill file") {
                        Ok(b) => b,
                        Err(e) => return Some(Err(e)),
                    },
                    None => data,
                };
                at = 0;
            }
            let row = || {
                let head = block.get(at..at + 12)?;
                let seq = u64::from_le_bytes(head[..8].try_into().unwrap());
                let end = at + 12 + u32::from_le_bytes(head[8..].try_into().unwrap()) as usize;
                let row = unpack(std::str::from_utf8(block.get(at + 12..end)?).ok()?, width, None)?;
                Some((seq, row, end))
            };
            Some(
                row()
                    .map(|(seq, row, end)| {
                        at = end;
                        (seq, row)
                    })
                    .ok_or_else(bad),
            )
        }))
    }
}

/// A file read from a place of its own (`.1`), whoever else reads it.
struct At<'a>(&'a File, u64);

impl Read for At<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let n = self.0.read_at(buf, self.1)?;
        self.1 += n as u64;
        Ok(n)
    }
}

/// One table's rows for an ORDER BY, gathered from parts that arrive in any order (see
/// `Exec::select_one`). Parts hand over their rows with their places in the table (with a LIMIT,
/// only their best `keep`). While the rows fit in CHRONOS_WORK_MEM they're only held, and
/// `finish` gives them back in table order for the usual in-memory sort. Past that, the held
/// rows become a run on disk: each row's ORDER BY keys and output, sorted; `finish` merges the
/// runs. Ties keep table order either way.
pub(crate) struct Sorter<'d> {
    dirs: &'d [Dir],
    keep: Option<usize>,
    key: &'d RowFn<'d>,
    out: &'d RowFn<'d>,
    held: Vec<(Vec<u64>, Batch)>, // rows with their places
    count: usize,
    bytes: usize,
    runs: Vec<Spill>,
}

/// A row's ORDER BY keys, or its output.
pub(crate) type RowFn<'a> = dyn Fn(&[Value]) -> super::Result<Vec<Value>> + Sync + 'a;

type Entry = (u64, Vec<Value>, Vec<Value>);

/// What a `Sorter` ends with.
pub(crate) enum Sorted {
    /// every row held, in table order: to sort as usual
    Rows(Batch),
    /// the output rows, sorted (after spilling)
    Outs(Vec<Vec<Value>>),
}

impl<'d> Sorter<'d> {
    pub fn new(dirs: &'d [Dir], keep: Option<usize>, key: &'d RowFn<'d>, out: &'d RowFn<'d>) -> Sorter<'d> {
        Sorter { dirs, keep, key, out, held: vec![], count: 0, bytes: 0, runs: vec![] }
    }

    /// The held rows in table order (parts come in any order; each is in order already).
    fn in_order(&mut self, width: usize) -> (Vec<u64>, Batch) {
        let mut held = std::mem::take(&mut self.held);
        held.sort_unstable_by_key(|(at, _)| at.first().copied().unwrap_or(0));
        let mut all = Batch::new(width);
        all.vals.reserve(held.iter().map(|(_, b)| b.vals.len()).sum());
        let mut at = Vec::with_capacity(self.count);
        let ordered = held.windows(2).all(|w| w[0].0.last() <= w[1].0.first());
        for (a, b) in held {
            at.extend(a);
            all.append(b);
        }
        if !ordered {
            // a LIMIT's best-so-far mixes places: put them back in order
            let mut idx: Vec<usize> = (0..at.len()).collect();
            idx.sort_unstable_by_key(|&i| at[i]);
            let mut b = Batch::new(width);
            for &i in &idx {
                b.push().clone_from_slice(all.row(i));
            }
            at = idx.into_iter().map(|i| at[i]).collect();
            all = b;
        }
        self.count = 0;
        (at, all)
    }

    /// A part's rows with their places (in table order), and about how much memory they take.
    pub fn add(&mut self, at: Vec<u64>, rows: Batch, bytes: usize) -> super::Result<()> {
        let width = rows.width;
        self.count += rows.len;
        self.bytes += bytes;
        self.held.push((at, rows));
        if let Some(k) = self.keep {
            if self.count > 4 * k.max(256) {
                // only the best `keep` so far, in table order
                let (at, rows) = self.in_order(width);
                let keys = rows.rows().map(self.key).collect::<super::Result<Vec<_>>>()?;
                let mut best = order_positions(keys.len(), &|i| &keys[i], self.dirs, Some(k))?;
                best.sort_unstable();
                let mut b = Batch::new(width);
                for &i in &best {
                    b.push().clone_from_slice(rows.row(i));
                }
                self.count = b.len;
                self.held.push((best.into_iter().map(|i| at[i]).collect(), b));
            }
            return Ok(());
        }
        if self.bytes > work_mem() {
            let run = self.entries(width)?;
            let w = run.first().map_or(0, |e| e.1.len() + e.2.len());
            let mut f = Spill::new(w).map_err(io)?;
            for (seq, k, o) in run {
                f.write(seq, &[k, o].concat()).map_err(io)?;
            }
            self.runs.push(f);
            self.bytes = 0;
        }
        Ok(())
    }

    /// The held rows as sorted entries: their places, keys and outputs.
    fn entries(&mut self, width: usize) -> super::Result<Vec<Entry>> {
        let (at, rows) = self.in_order(width);
        let keys = rows.rows().map(self.key).collect::<super::Result<Vec<_>>>()?;
        let order = order_positions(keys.len(), &|i| &keys[i], self.dirs, None)?;
        let mut keys: Vec<Option<Vec<Value>>> = keys.into_iter().map(Some).collect();
        order.into_iter().map(|i| Ok((at[i], keys[i].take().unwrap_or_default(), (self.out)(rows.row(i))?))).collect()
    }

    pub fn finish(mut self, width: usize) -> super::Result<Sorted> {
        if self.runs.is_empty() {
            return Ok(Sorted::Rows(self.in_order(width).1));
        }
        // merge the runs and what's held, each sorted: the smallest head next
        let held = self.entries(width)?;
        let nkeys = self.dirs.len();
        let mut sources: Vec<Box<dyn Iterator<Item = super::Result<Entry>> + '_>> =
            vec![Box::new(held.into_iter().map(Ok))];
        for r in &mut self.runs {
            sources.push(Box::new(r.read().map_err(io)?.map(move |x| {
                let (seq, mut row) = x.map_err(io)?;
                let out = row.split_off(nkeys);
                Ok((seq, row, out))
            })));
        }
        let dirs = self.dirs;
        let before = |a: &Entry, b: &Entry| order_cmp(&a.1, &b.1, dirs).then(a.0.cmp(&b.0)) == std::cmp::Ordering::Less;
        let mut heads: Vec<Option<Entry>> =
            sources.iter_mut().map(|s| s.next().transpose()).collect::<super::Result<_>>()?;
        let mut out = vec![];
        // ponytail: picks the head by scanning them all; a heap if there are ever many runs
        loop {
            let mut best: Option<usize> = None;
            for (i, h) in heads.iter().enumerate() {
                if let Some(h) = h
                    && best.is_none_or(|b| before(h, heads[b].as_ref().unwrap()))
                {
                    best = Some(i);
                }
            }
            let Some(b) = best else { break };
            let e = std::mem::replace(&mut heads[b], sources[b].next().transpose()?).unwrap();
            out.push(e.2);
        }
        Ok(Sorted::Outs(out))
    }
}

/// Key-value pairs, sorted by key then value within a memory budget however many there are:
/// held while they fit, past that written out as sorted runs, merged as they're read (building
/// an index over a big table).
pub(crate) struct Pairs {
    held: Vec<(String, String)>,
    bytes: usize,
    budget: usize,
    runs: Vec<Spill>,
}

impl Pairs {
    pub fn new(budget: usize) -> Pairs {
        Pairs { held: vec![], bytes: 0, budget, runs: vec![] }
    }

    pub fn push(&mut self, k: String, v: String) -> super::Result<()> {
        self.bytes += k.len() + v.len() + 64; // two strings' headers and allocations
        self.held.push((k, v));
        if self.bytes > self.budget {
            self.held.sort_unstable();
            let mut f = Spill::new(2).map_err(io)?;
            for (k, v) in self.held.drain(..) {
                f.write(0, &[Value::Text(k), Value::Text(v)]).map_err(io)?;
            }
            self.runs.push(f);
            self.bytes = 0;
        }
        Ok(())
    }

    /// Calls `f` on every pair, in order, handing each over: they are gone from `self` (which spilled ones are
    /// not: those are read back, as `each` reads them). Cheaper than `each` when they were all held.
    pub fn drain(&mut self, mut f: impl FnMut(String, String) -> super::Result<()>) -> super::Result<()> {
        if self.runs.is_empty() {
            self.held.sort_unstable();
            self.bytes = 0;
            for (k, v) in self.held.drain(..) {
                f(k, v)?;
            }
            return Ok(());
        }
        self.each(|k, v| f(k.to_string(), v.to_string()))
    }

    /// Calls `f` on every pair, in order (again each time it's called).
    pub fn each(&mut self, mut f: impl FnMut(&str, &str) -> super::Result<()>) -> super::Result<()> {
        use std::cmp::Reverse;
        self.held.sort_unstable();
        // (all of them held: no runs to merge, and no copies)
        if self.runs.is_empty() {
            for (k, v) in &self.held {
                f(k, v)?;
            }
            return Ok(());
        }
        type Source<'a> = Box<dyn Iterator<Item = super::Result<(String, String)>> + 'a>;
        let mut sources: Vec<Source> = vec![Box::new(self.held.iter().map(|(k, v)| Ok((k.clone(), v.clone()))))];
        for r in &mut self.runs {
            sources.push(Box::new(r.read().map_err(io)?.map(|x| match x.map_err(io)?.1.as_mut_slice() {
                [Value::Text(k), Value::Text(v)] => Ok((std::mem::take(k), std::mem::take(v))),
                _ => Err(io(io::Error::new(io::ErrorKind::InvalidData, "a spill file is damaged"))),
            })));
        }
        // the smallest head next
        let mut heads = std::collections::BinaryHeap::new();
        for (i, s) in sources.iter_mut().enumerate() {
            if let Some(x) = s.next() {
                let (k, v) = x?;
                heads.push(Reverse((k, v, i)));
            }
        }
        while let Some(Reverse((k, v, i))) = heads.pop() {
            f(&k, &v)?;
            if let Some(x) = sources[i].next() {
                let (k, v) = x?;
                heads.push(Reverse((k, v, i)));
            }
        }
        Ok(())
    }
}

fn io(e: io::Error) -> super::SqlError {
    super::SqlError { code: "53100", message: format!("spilling to disk: {e}"), info: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_round_trip_and_the_file_goes() {
        let mut s = Spill::new(3).unwrap();
        let rows = [
            vec![Value::Int(1), Value::Text("a:b\n".into()), Value::Null],
            vec![Value::Float(-2.5), Value::Null, Value::Bool(true)],
        ];
        for (i, r) in rows.iter().enumerate() {
            s.write(i as u64 * 7, r).unwrap();
        }
        for _ in 0..2 {
            let back: Vec<(u64, Vec<Value>)> = s.read().unwrap().collect::<io::Result<_>>().unwrap();
            assert_eq!(back, vec![(0, rows[0].clone()), (7, rows[1].clone())]);
        }
        // no name in the folder: unlinked as it was made, and private while it had one
        use std::os::unix::fs::MetadataExt;
        assert_eq!(s.file.metadata().unwrap().nlink(), 0);
        let big: Vec<Value> = vec![Value::Text("x".repeat(1000)), Value::Null, Value::Int(3)];
        let mut s = Spill::new(3).unwrap();
        for i in 0..200 {
            s.write(i, &big).unwrap(); // several blocks
        }
        assert_eq!(s.read().unwrap().count(), 200);
        assert_eq!(parse_size("64MB"), Some(64 << 20));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn pairs_come_back_sorted_from_runs_and_memory() {
        let mut p = Pairs::new(1000); // spills every ~12 pairs
        let mut want = vec![];
        let mut r = 7u64;
        for i in 0..500 {
            r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let (k, v) = (format!("k\0{}", r % 97), format!("{i}"));
            want.push((k.clone(), v.clone()));
            p.push(k, v).unwrap();
        }
        assert!(p.runs.len() > 10);
        want.sort();
        for _ in 0..2 {
            let mut got = vec![];
            p.each(|k, v| {
                got.push((k.to_string(), v.to_string()));
                Ok(())
            })
            .unwrap();
            assert_eq!(got, want);
        }
    }
}
