//! Backups of a running database, checking a database's files, and starting from a backup.
//!
//! A backup is a database folder, consistent as of the moment it started: a checkpoint taken
//! then, its pages, and the log up to that moment, closed as a clean shutdown closes it (so worlds come
//! back with their unmerged work). History comes too, so a restored database goes back in time as
//! far as the original could. Writers keep going while it copies.
//! ponytail: checkpoints wait for the copy, and writers wait for them once the log passes 256 MB;
//! copy sealed packs before taking the lock if copies get that slow. No incremental backups: each
//! is a full copy.

use crate::{Core, Db, disk, prolly};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;

/// Takes `f`'s advisory lock without waiting: whether it was free. The lock goes with the process
/// that holds it, however it ends (kill -9 too), which is what tells a backup under way from the
/// leftovers of one that was interrupted.
fn try_lock(f: &File) -> bool {
    // SAFETY: flock on a descriptor `f` keeps open
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Serializes making a backup's folder (and locking it) with sweeping for stale ones, so a sweep
/// never meets a folder that is new but not yet locked.
static CLAIM: Mutex<()> = Mutex::new(());

/// The file a backup makes first in its temporary folder: what marks the folder as Chronos's. A
/// folder named like one (`.NAME.partial`) without it is someone else's and is never removed.
const MARK: &str = ".chronos-backup-partial";

/// Whether `dir` is a temporary backup folder Chronos made (it holds `MARK`).
fn ours(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(MARK)).is_ok_and(|m| m.is_file())
}

/// Whether a backup may take `dir` over: Chronos's (see `ours`), or empty and over a minute old, which is
/// what a backup killed between making the folder and marking it leaves (removing an empty folder loses
/// nothing, whoever made it; the minute leaves alone one another process has only just made, before it
/// locks and marks it).
fn reclaimable(dir: &Path) -> bool {
    let empty = std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none());
    let old = std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > std::time::Duration::from_secs(60)));
    ours(dir) || (empty && old)
}

/// Makes the temporary folder `tmp` of a backup, empty and marked as Chronos's, and returns it
/// locked until dropped. A folder of that name left by an interrupted backup is removed; so are the
/// others like it (`.NAME.partial`, with Chronos's mark, or empty) beside it that no backup holds:
/// those of backups that were killed. A live backup (this process's or another's) holds its folder's
/// lock, and is left alone; a folder with files but without the mark is not Chronos's, and is never
/// touched.
fn claim_partial(tmp: &Path) -> io::Result<File> {
    let _one = CLAIM.lock().unwrap_or_else(|p| p.into_inner());
    if std::fs::symlink_metadata(tmp).is_ok() {
        if !reclaimable(tmp) {
            return Err(io::Error::other(format!(
                "{} already exists and was not made by a Chronos backup: move it away",
                tmp.display()
            )));
        }
        let old = File::open(tmp)?;
        if !try_lock(&old) {
            return Err(io::Error::other(format!("a backup to {} is already running", tmp.display())));
        }
        std::fs::remove_dir_all(tmp)?;
    }
    std::fs::create_dir_all(tmp)?;
    let held = File::open(tmp)?;
    if !try_lock(&held) {
        return Err(io::Error::other(format!("a backup to {} is already running", tmp.display())));
    }
    File::create(tmp.join(MARK))?.sync_all()?;
    let parent = tmp.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    for e in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        let (path, name) = (e.path(), e.file_name());
        let stale = name.to_str().is_some_and(|n| n.starts_with('.') && n.ends_with(".partial"))
            && e.file_type().is_ok_and(|t| t.is_dir())
            && path != tmp
            && reclaimable(&path);
        if stale && File::open(&path).is_ok_and(|f| try_lock(&f)) && std::fs::remove_dir_all(&path).is_ok() {
            eprintln!("chronos: removed {}, left by an interrupted backup", path.display());
        }
    }
    Ok(held)
}

/// What a backup copied.
#[derive(Debug, Clone, PartialEq)]
pub struct Backup {
    pub files: usize,
    pub bytes: u64,
}

/// What `verify` read: every page still needed (each checked against its hash) and every log record.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub pages: usize,
    pub bytes: u64,
    pub log_records: u64,
}

impl Core {
    /// Copies the database to `to`, a new or empty folder, while it keeps running. The copy is
    /// written beside `to` and renamed into place at the end, so a failed backup leaves nothing
    /// that looks like one. Open the backup with `Db::open`, or start a folder from it with
    /// `Db::from_backup`.
    pub fn backup(&self, to: impl AsRef<Path>) -> io::Result<Backup> {
        let to = to.as_ref();
        let (Some(log), None) = (&self.log, self.remote()) else {
            return Err(io::Error::other(match self.remote() {
                Some(_) => "a cloud-backed database's bucket is its backup (see docs/operations.md)",
                None => "an in-memory database has nothing on disk to back up",
            }));
        };
        if std::fs::read_dir(to).is_ok_and(|mut d| d.next().is_some()) {
            return Err(io::Error::other(format!("{} is not empty: back up into a new folder", to.display())));
        }
        let name = to.file_name().ok_or_else(|| io::Error::other(format!("{} is not a folder name", to.display())))?;
        let tmp = to.with_file_name(format!(".{}.partial", name.to_string_lossy()));
        let _hold = claim_partial(&tmp)?; // (its lock lasts until the backup is renamed into place)
        let copied = (|| {
            let _one = log.ckpt.lock().unwrap(); // no other checkpoint or cleanup changes files meanwhile
            self.checkpoint_now(log, false)?; // a manifest to start from, and little log to copy
            self.flush_batches(); // open worlds' writes since
            let cut = log.cut()?;
            disk::copy_database(&log.dir, &tmp, log.crypt.as_deref(), Some(cut))
        })();
        let (files, bytes) = copied.inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&tmp);
        })?;
        let _ = std::fs::remove_dir(to); // empty, if it exists
        std::fs::rename(&tmp, to)?;
        // (the finished backup is not a partial one: the mark goes once it has its name, so a backup stopped before
        // the rename leaves a folder the next one may reclaim, and one stopped after it a backup with a stray mark)
        std::fs::remove_file(to.join(MARK))?;
        if let Some(parent) = to.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(Backup { files, bytes })
    }

    /// Reads every page any world, reader or kept history still needs, checking each against its
    /// hash, and every log record against its checksum. An error names what is missing or corrupt.
    pub fn verify(&self) -> io::Result<Verified> {
        let (Some(log), Some(pages)) = (&self.log, &self.pages) else {
            return Ok(Verified { pages: 0, bytes: 0, log_records: 0 });
        };
        let _one = log.ckpt.lock().unwrap();
        let (mut roots, history) = self.roots()?;
        roots.extend(history);
        // (a page the pack's index lacks is one whose record was damaged, or whose pack is gone)
        let lost = |e: io::Error| match e.kind() {
            io::ErrorKind::NotFound => io::Error::new(e.kind(), format!("{e}; its pack is corrupt or lost")),
            _ => e,
        };
        let live = prolly::reachable(pages, &roots).map_err(lost)?;
        let mut bytes = 0;
        for h in &live {
            bytes += pages.check(h).map_err(lost)?;
        }
        log.cut()?;
        Ok(Verified { pages: live.len(), bytes, log_records: disk::check_log(&log.dir, log.crypt.as_deref())? })
    }
}

impl Db {
    /// Starts the database folder `dir` (new or empty) from a backup made by `backup` (or any
    /// closed database folder), then opens and verifies it. The backup is only read. An encrypted
    /// backup stays encrypted and needs its `key`.
    pub fn from_backup(backup: impl AsRef<Path>, dir: impl AsRef<Path>, key: Option<&[u8; 32]>) -> io::Result<Db> {
        let (backup, dir) = (backup.as_ref(), dir.as_ref());
        if !backup.join("manifest").exists() {
            return Err(io::Error::other(format!("{} is not a Chronos backup: it has no manifest", backup.display())));
        }
        if std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some()) {
            return Err(io::Error::other(format!("{} is not empty: restore into a new folder", dir.display())));
        }
        {
            let _lock = disk::lock(backup)?; // nobody has it open, nor opens it while we copy
            disk::copy_database(backup, dir, None, None)?;
        }
        let db = Db::open_with(dir, &crate::Open { key: key.copied(), ..Default::default() })?;
        db.verify()?;
        Ok(db)
    }
}
