//! Statements too big to hold in memory.
//!
//! A statement holds its writes in memory until they pass `CHRONOS_WORK_MEM`, and then commits as
//! before. Past that, an INSERT (VALUES, SELECT or COPY), UPDATE or DELETE that can
//! (`Exec::bulk_ok`) goes on in parts of rows, and each part's writes go to a sorted run on disk
//! (`<database>/spill`, lz4-compressed, and sealed with the database's key if it has one), so
//! memory holds one part however big the statement is:
//!
//! - **Checks.** Each part is checked against the branch as the statement found it (`start`),
//!   as `enforce` checks a whole statement. What depends on other parts is marked in the runs
//!   instead: a row key an INSERT adds (`NEW`) or an UPDATE moves a row to (`CLAIM`), a unique
//!   value a row takes (`CLAIM`) and one it lets go of (`FREE`).
//! - **Commit.** The runs are merged in key order. Per key, a value written wins over a delete
//!   (a statement deletes before it writes, as `enforce` orders them); two claims, or a claim of
//!   a key `start` holds for another row that nothing in the statement frees, are a violation,
//!   and nothing is written. The merged changes go into a new tree built beside the branch's, a
//!   batch at a time, then a checkpoint makes it the branch's state (`Core::commit_bulk`): readers
//!   see all of the statement or none of it, a crash before the checkpoint's note leaves nothing,
//!   and after it everything, and nothing is left in memory afterwards.
//! - **Cleanup.** Runs are deleted when the statement ends however it ends, and the spill folder
//!   is emptied when the database opens; pages of a tree never committed are freed by cleanup.

use super::{Result, SqlError, View};
use crate::Val;
use crate::crypt::Crypt;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// A write of a value.
pub(super) const PUT: u8 = 0;
/// A write that deletes.
pub(super) const DEL: u8 = 1;
/// A row key an INSERT adds, already checked against `start`: a second claim is a clash.
pub(super) const NEW: u8 = 2;
/// A key a row takes (a moved row's key, a unique value): a second claim is a clash, and so is
/// `start` holding it for another row unless the statement frees it.
pub(super) const CLAIM: u8 = 3;
/// A unique value a row lets go of.
pub(super) const FREE: u8 = 4;

const BLOCK: usize = 64 << 10;
/// Changes handed to the tree at a time.
const BATCH_BYTES: usize = 8 << 20;

type Entry = (String, u8, Option<String>);
/// A merge's next entry: key, source, kind, value.
type Head = Reverse<(String, usize, u8, Option<String>)>;
/// A key's entries: kinds and values.
type Group = Vec<(u8, Option<String>)>;

/// One part's writes (or several parts', merged), sorted by key, in a file of blocks.
struct Run {
    path: PathBuf,
    crypt: Option<Arc<Crypt>>,
}

/// A run being written, an entry at a time in key order.
struct Writer {
    run: Run,
    out: BufWriter<File>,
    block: Vec<u8>,
}

impl Writer {
    fn new(dir: &std::path::Path, crypt: Option<Arc<Crypt>>) -> io::Result<Writer> {
        std::fs::create_dir_all(dir)?;
        let (file, path) = super::spill::create(dir, "run")?;
        let out = BufWriter::with_capacity(1 << 16, file);
        Ok(Writer { run: Run { path, crypt }, out, block: Vec::with_capacity(BLOCK + (16 << 10)) })
    }

    fn push(&mut self, k: &str, kind: u8, v: Option<&str>) -> io::Result<()> {
        let b = &mut self.block;
        b.push(kind);
        b.extend((k.len() as u32).to_le_bytes());
        b.extend(k.as_bytes());
        match v {
            Some(v) => {
                b.extend((v.len() as u32).to_le_bytes());
                b.extend(v.as_bytes());
            }
            None => b.extend(u32::MAX.to_le_bytes()),
        }
        if b.len() >= BLOCK {
            self.run.put(&mut self.out, &self.block)?;
            self.block.clear();
        }
        Ok(())
    }

    fn finish(mut self) -> io::Result<Run> {
        if !self.block.is_empty() {
            self.run.put(&mut self.out, &self.block)?;
        }
        self.out.flush()?;
        super::spill::SPILLED.fetch_add(std::fs::metadata(&self.run.path)?.len(), Ordering::Relaxed);
        Ok(self.run)
    }
}

impl Run {
    fn write(dir: &std::path::Path, crypt: Option<Arc<Crypt>>, entries: &mut [Entry]) -> io::Result<Run> {
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut w = Writer::new(dir, crypt)?;
        for (k, kind, v) in entries.iter() {
            w.push(k, *kind, v.as_deref())?;
        }
        w.finish()
    }

    fn put(&self, out: &mut impl Write, block: &[u8]) -> io::Result<()> {
        let packed = lz4_flex::compress_prepend_size(block);
        let sealed = match &self.crypt {
            Some(c) => c.seal(b"sonos spill", &packed),
            None => packed,
        };
        out.write_all(&(sealed.len() as u32).to_le_bytes())?;
        out.write_all(&sealed)
    }

    /// Its entries, in key order.
    fn read(&self) -> io::Result<impl Iterator<Item = io::Result<Entry>> + '_> {
        let mut r = BufReader::with_capacity(16 << 10, File::open(&self.path)?);
        let mut held: std::vec::IntoIter<Entry> = vec![].into_iter();
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "a spill file is damaged");
        Ok(std::iter::from_fn(move || {
            loop {
                if let Some(e) = held.next() {
                    return Some(Ok(e));
                }
                let mut len = [0u8; 4];
                match r.read_exact(&mut len) {
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return None,
                    Err(e) => return Some(Err(e)),
                    Ok(()) => {}
                }
                let mut sealed = vec![0u8; u32::from_le_bytes(len) as usize];
                if let Err(e) = r.read_exact(&mut sealed) {
                    return Some(Err(e));
                }
                let block = match &self.crypt {
                    Some(c) => match c.open(b"sonos spill", &sealed, "a spill file") {
                        Ok(b) => b,
                        Err(e) => return Some(Err(e)),
                    },
                    None => sealed,
                };
                let Ok(block) = lz4_flex::decompress_size_prepended(&block) else { return Some(Err(bad())) };
                match parse(&block) {
                    Some(es) => held = es.into_iter(),
                    None => return Some(Err(bad())),
                }
            }
        }))
    }
}

fn parse(mut b: &[u8]) -> Option<Vec<Entry>> {
    let mut out = vec![];
    let u32_ = |b: &mut &[u8]| -> Option<u32> {
        let (n, rest) = b.split_first_chunk::<4>()?;
        *b = rest;
        Some(u32::from_le_bytes(*n))
    };
    while let Some((&kind, rest)) = b.split_first() {
        b = rest;
        let n = u32_(&mut b)? as usize;
        let k = String::from_utf8(b.get(..n)?.to_vec()).ok()?;
        b = &b[n..];
        let v = match u32_(&mut b)? {
            u32::MAX => None,
            n => {
                let n = n as usize;
                let v = String::from_utf8(b.get(..n)?.to_vec()).ok()?;
                b = &b[n..];
                Some(v)
            }
        };
        out.push((k, kind, v));
    }
    Some(out)
}

type Source<'a> = Box<dyn Iterator<Item = io::Result<Entry>> + 'a>;

/// Runs' entries together, in key order (entries of one key in any order).
struct Merge<'a> {
    sources: Vec<Source<'a>>,
    heads: BinaryHeap<Head>,
}

impl<'a> Merge<'a> {
    fn new(runs: &'a [Run]) -> io::Result<Merge<'a>> {
        let mut m = Merge { sources: vec![], heads: BinaryHeap::new() };
        for r in runs {
            m.sources.push(Box::new(r.read()?));
            m.pull(m.sources.len() - 1)?;
        }
        Ok(m)
    }

    fn pull(&mut self, i: usize) -> io::Result<()> {
        if let Some(e) = self.sources[i].next() {
            let (k, kind, v) = e?;
            self.heads.push(Reverse((k, i, kind, v)));
        }
        Ok(())
    }

    fn next(&mut self) -> io::Result<Option<Entry>> {
        let Some(Reverse((k, i, kind, v))) = self.heads.pop() else { return Ok(None) };
        self.pull(i)?;
        Ok(Some((k, kind, v)))
    }

    /// The next key's entries: its kinds and values.
    fn group(&mut self) -> io::Result<Option<(String, Group)>> {
        let Some((key, kind, v)) = self.next()? else { return Ok(None) };
        let mut group = vec![(kind, v)];
        while self.heads.peek().is_some_and(|Reverse(h)| h.0 == key) {
            let (_, kind, v) = self.next()?.expect("peeked");
            group.push((kind, v));
        }
        Ok(Some((key, group)))
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Runs merged at a time, so a statement has few (and memory holds a block of each as it
/// merges): each merge of this many takes them a level up.
const FAN: usize = 16;

/// A statement's writes on their way to disk (see the module's comment).
pub(super) struct Spilled {
    dir: PathBuf,
    crypt: Option<Arc<Crypt>>,
    runs: Vec<(Run, u32)>, // with how many merges made each; levels never rise along the list
    writes: u64,
    /// of them, the tables' own rows (not the entries behind indexes and constraints): what history counts
    rows: u64,
    /// the parts that went straight into a tree beside the branch's (see `add`), and their last key
    direct: Option<crate::Tree>,
    last: Option<String>,
}

impl Spilled {
    /// None for a database in memory, which holds everything anyway.
    pub fn new(db: &crate::Core) -> Option<Spilled> {
        let (dir, crypt) = db.spill_dir()?;
        Some(Spilled { dir, crypt, runs: vec![], writes: 0, rows: 0, direct: None, last: None })
    }

    /// One part's writes, and its marks (`NEW`, `CLAIM`, `FREE`), checked against `start`.
    ///
    /// A part that only writes values and adds keys, every one after all the keys before it
    /// (a load in key order), can't meet another part's keys: merging it would change nothing,
    /// so while no part has needed a run, such parts go straight into a tree beside the branch's
    /// (`Core::bulk_apply`), written once and not again at the end.
    pub fn add(
        &mut self,
        db: &crate::Core,
        start: &View,
        mut writes: Vec<(String, Option<String>)>,
        marks: Vec<(String, u8)>,
    ) -> Result<()> {
        self.writes += writes.len() as u64;
        self.rows += writes.iter().filter(|w| !crate::reserved_table(&w.0)).count() as u64;
        if self.runs.is_empty() && writes.iter().all(|w| w.1.is_some()) && marks.iter().all(|m| m.1 == NEW) {
            writes.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let after = writes.first().is_some_and(|w| self.last.as_ref().is_none_or(|l| w.0 > *l));
            let ordered = writes.windows(2).all(|w| w[0].0 < w[1].0);
            let marked = marks.iter().all(|m| writes.binary_search_by(|w| w.0.cmp(&m.0)).is_ok());
            if after && ordered && marked {
                self.last = writes.last().map(|w| w.0.clone());
                let batch: crate::Changes = writes.into_iter().map(|(k, v)| (k, v.map(Val::from))).collect();
                self.direct = Some(db.bulk_apply(start, self.direct.take(), &batch)?);
                return Ok(());
            }
        }
        // a key this part adds that a part before it put straight into the tree: a clash
        if let (Some(tree), Some(last)) = (&self.direct, &self.last) {
            for (k, _) in marks.iter().filter(|m| m.1 == NEW && m.0 <= *last) {
                if let Some(had) = db.bulk_get(tree, k)? {
                    let now = writes.iter().find(|w| w.0 == *k).and_then(|w| w.1.clone()).unwrap_or_default();
                    return Err(super::clash(start, k, &[had.to_string(), now]));
                }
            }
        }
        let mut entries: Vec<Entry> = Vec::with_capacity(writes.len() + marks.len());
        entries.extend(writes.into_iter().map(|(k, v)| {
            let kind = if v.is_some() { PUT } else { DEL };
            (k, kind, v)
        }));
        entries.extend(marks.into_iter().map(|(k, kind)| (k, kind, None)));
        if entries.is_empty() {
            return Ok(());
        }
        self.runs.push((Run::write(&self.dir, self.crypt.clone(), &mut entries).map_err(io)?, 0));
        drop(entries);
        // FAN runs of one level make one of the next
        while self.runs.len() >= FAN
            && self.runs[self.runs.len() - FAN..].iter().all(|r| r.1 == self.runs[self.runs.len() - 1].1)
        {
            self.merge_last(FAN)?;
        }
        Ok(())
    }

    /// Merges the last `n` runs (the smallest) into one, a level up from the highest of them.
    fn merge_last(&mut self, n: usize) -> Result<()> {
        super::tick()?;
        let at = self.runs.len() - n;
        let level = self.runs[at..].iter().map(|r| r.1).max().unwrap_or(0) + 1;
        let these: Vec<Run> = self.runs.drain(at..).map(|r| r.0).collect();
        let mut m = Merge::new(&these).map_err(io)?;
        let mut w = Writer::new(&self.dir, self.crypt.clone()).map_err(io)?;
        while let Some((k, kind, v)) = m.next().map_err(io)? {
            w.push(&k, kind, v.as_deref()).map_err(io)?;
        }
        drop(m);
        self.runs.push((w.finish().map_err(io)?, level));
        Ok(())
    }

    /// Merges the runs, checks what the marks ask for against `start` (the branch as the
    /// statement found it: `clash` says what's wrong with a key two rows claim, given their
    /// values), and commits the result. Returns the branch's version.
    pub fn commit(
        mut self,
        db: &crate::Core,
        start: &View,
        clash: &dyn Fn(&str, &[String]) -> SqlError,
    ) -> Result<u64> {
        // at most FAN at once, whatever the levels: memory holds a block of each
        while self.runs.len() > FAN {
            let n = (self.runs.len() - FAN + 1).min(FAN);
            self.merge_last(n)?;
        }
        let runs: Vec<Run> = self.runs.into_iter().map(|r| r.0).collect();
        let mut m = Merge::new(&runs).map_err(io)?;
        let mut failed: Option<SqlError> = None;
        let mut next = || -> crate::Result<Option<crate::Changes>> {
            let (mut batch, mut bytes) = (vec![], 0);
            while bytes < BATCH_BYTES {
                let Some((key, group)) = m.group()? else { break };
                super::tick().map_err(sql_to_core)?;
                match resolve(start, &key, group, clash) {
                    Ok(Some(value)) => {
                        bytes += key.len() + value.as_ref().map_or(0, |v| v.len()) + 64;
                        batch.push((key, value));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        failed = Some(e);
                        return Err(crate::Error::Constraint { code: "23505", message: String::new() });
                    }
                }
            }
            Ok((!batch.is_empty()).then_some(batch))
        };
        match db.commit_bulk(start, self.writes, self.rows, self.direct.take(), &mut next) {
            Ok(version) => Ok(version),
            Err(_) if failed.is_some() => Err(failed.unwrap()),
            Err(crate::Error::Constraint { code, message }) => Err(SqlError { code, message, info: None }),
            Err(e) => Err(e.into()),
        }
    }
}

/// What one key ends as, from everything the statement wrote and marked for it: None writes
/// nothing (marks alone).
fn resolve(
    start: &View,
    key: &str,
    group: Group,
    clash: &dyn Fn(&str, &[String]) -> SqlError,
) -> Result<Option<Option<Val>>> {
    let (mut put, mut deleted, mut claims, mut check, mut freed) = (vec![], false, 0, false, false);
    for (kind, v) in group {
        match kind {
            PUT => put.extend(v),
            DEL => deleted = true,
            NEW => claims += 1,
            CLAIM => {
                claims += 1;
                check = true;
            }
            _ => freed = true,
        }
    }
    if claims > 1 {
        return Err(clash(key, &put));
    }
    if check && !deleted && !freed {
        // a key the branch holds already: taken from a row the statement didn't move off it
        if let Some(had) = start.get(key)? {
            let row_key = !key.starts_with("_sonos_");
            if row_key || put.first().is_none_or(|p| **p != *had) {
                return Err(clash(key, &[had.to_string(), put.first().cloned().unwrap_or_default()]));
            }
        }
    }
    Ok(match put.pop() {
        Some(v) => Some(Some(Val::from(v))),
        None if deleted => Some(None),
        None => None,
    })
}

fn io(e: io::Error) -> SqlError {
    SqlError { code: "53100", message: format!("spilling to disk: {e}"), info: None }
}

fn sql_to_core(e: SqlError) -> crate::Error {
    crate::Error::Constraint { code: e.code, message: e.message }
}
