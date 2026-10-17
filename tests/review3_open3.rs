//! Third review, open and log: a refused or failed open leaves the log exactly as it found it (no
//! Close record: the anchor can't be walked forward, a crash stays a crash), damage after the
//! last synced position is a crash tail and damage before it is corruption, a torn tail full of
//! plausible frame headers opens quickly, settings entries this Chronos doesn't know survive a
//! write, and (with `--features s3`) a takeover opens under an anchor.

use chronos::{Db, MAIN, Open, Resolve, Setting, SyncCommit};
use std::fs;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-review3-open3-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    let _ = fs::remove_file(anchor_of(&d));
    d
}

fn anchor_of(dir: &Path) -> PathBuf {
    dir.with_extension("anchor")
}

fn anchored(dir: &Path) -> Open {
    Open { anchor: Some(anchor_of(dir)), ..Default::default() }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dst = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dst);
        } else {
            fs::copy(&p, dst).unwrap();
        }
    }
}

fn last_segment(dir: &Path) -> PathBuf {
    let mut segs: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("wal-"))
        .collect();
    segs.sort();
    segs.pop().unwrap()
}

/// Where each frame of a segment ends, up to the zeros written ahead of them.
fn frame_ends(b: &[u8]) -> Vec<usize> {
    let (mut off, mut ends) = (0, vec![]);
    while off + 8 <= b.len() {
        let len = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
        if len == 0 {
            break;
        }
        off += 8 + len;
        ends.push(off);
    }
    ends
}

/// A segment's records, without the zeros written ahead of them (recovery trims those).
fn records(seg: &Path) -> Vec<u8> {
    let b = fs::read(seg).unwrap();
    b[..frame_ends(&b).last().copied().unwrap_or(0)].to_vec()
}

fn cleanup(dirs: &[&Path]) {
    for d in dirs {
        let _ = fs::remove_dir_all(d);
        let _ = fs::remove_file(anchor_of(d));
    }
}

#[test]
fn refused_opens_do_not_move_the_log_forward() {
    let (dir, old) = (tmp("refused"), tmp("refused-old"));
    {
        let db = Db::open_with(&dir, &anchored(&dir)).unwrap();
        db.put(MAIN, "a", "1").unwrap();
    }
    copy_dir(&dir, &old);
    {
        let db = Db::open_with(&dir, &anchored(&dir)).unwrap();
        db.put(MAIN, "b", "2").unwrap();
    }
    fs::remove_dir_all(&dir).unwrap();
    copy_dir(&old, &dir); // rolled back
    let seg = last_segment(&dir);
    let before = records(&seg);
    for _ in 0..50 {
        let e = Db::open_with(&dir, &anchored(&dir)).err().expect("a rolled-back folder must be refused");
        assert!(e.to_string().contains("older than its anchor"), "{e}");
    }
    assert_eq!(records(&seg), before, "a refused open wrote to the log");
    cleanup(&[&dir, &old]);
}

#[test]
fn a_failed_open_after_a_crash_keeps_the_crash() {
    let (dir, crashed) = (tmp("failed"), tmp("failed-copy"));
    let db = Db::open(&dir).unwrap();
    db.fork(MAIN, "agent").unwrap();
    db.put(MAIN, "sync", "point").unwrap();
    db.put("agent", "work", "done").unwrap(); // buffered, not synced
    copy_dir(&dir, &crashed); // kill -9 now
    drop(db);
    fs::write(crashed.join("settings"), "not json").unwrap();
    let seg = last_segment(&crashed);
    let before = records(&seg);
    let e = Db::open(&crashed).err().expect("a corrupt settings file fails the open");
    assert!(e.to_string().contains("corrupt"), "{e}");
    assert_eq!(records(&seg), before, "a failed open wrote to the log");
    fs::remove_file(crashed.join("settings")).unwrap();
    let db = Db::open(&crashed).unwrap();
    assert!(
        matches!(db.merge("agent", Resolve::Fail), Err(chronos::Error::Stale { .. })),
        "the crash must still be reported after the failed open"
    );
    drop(db);
    cleanup(&[&dir, &crashed]);
}

/// Writes synced then unsynced records, and returns a crashed copy of the folder, its last
/// segment's bytes and where the synced records end.
fn synced_then_not(name: &str) -> (PathBuf, PathBuf, Vec<u8>, usize) {
    let (dir, crashed) = (tmp(name), tmp(&format!("{name}-copy")));
    let db = Db::open(&dir).unwrap();
    for i in 0..5 {
        db.put(MAIN, &format!("s{i}"), "synced").unwrap();
    }
    let synced = frame_ends(&fs::read(last_segment(&dir)).unwrap()).pop().unwrap();
    db.set_sync_commit(SyncCommit::Off).unwrap();
    for i in 0..5 {
        db.put(MAIN, &format!("u{i}"), "unsynced").unwrap();
    }
    copy_dir(&dir, &crashed);
    drop(db);
    let bytes = fs::read(last_segment(&crashed)).unwrap();
    (dir, crashed, bytes, synced)
}

#[test]
fn a_gap_after_the_synced_position_is_a_crash_tail() {
    let (dir, crashed, mut bytes, synced) = synced_then_not("gap");
    let ends = frame_ends(&bytes);
    let i = ends.iter().position(|&e| e == synced).unwrap();
    // unsynced writes reached the disk out of order: one record never did, later ones did
    bytes[ends[i]..ends[i + 1]].fill(0);
    fs::write(last_segment(&crashed), &bytes).unwrap();
    let db = Db::open(&crashed).expect("a torn unsynced tail must open");
    for i in 0..5 {
        assert_eq!(db.get(MAIN, &format!("s{i}")).unwrap().as_deref(), Some("synced"));
    }
    drop(db);
    cleanup(&[&dir, &crashed]);
}

#[test]
fn a_gap_before_the_synced_position_is_corruption() {
    let (dir, crashed, mut bytes, synced) = synced_then_not("gap-synced");
    let ends = frame_ends(&bytes);
    let i = ends.iter().position(|&e| e == synced).unwrap();
    bytes[ends[i - 3]..ends[i - 2]].fill(0);
    fs::write(last_segment(&crashed), &bytes).unwrap();
    let e = Db::open(&crashed).err().expect("damage in synced records is corruption");
    assert!(e.to_string().contains("is corrupt at byte"), "{e}");
    cleanup(&[&dir, &crashed]);
}

#[test]
fn a_torn_tail_of_plausible_headers_opens_quickly() {
    let (dir, crashed, mut bytes, _) = synced_then_not("scan");
    let end = *frame_ends(&bytes).last().unwrap();
    // 4 MB where every 4th offset reads as a 60 kB frame: the old scan hashed 60 kB at each
    let junk: Vec<u8> = (0..4 << 20).map(|i| if i % 4 == 1 { 0xF0 } else { 0 }).collect();
    bytes.truncate(end);
    bytes.push(1); // a torn header
    bytes.extend(junk);
    fs::write(last_segment(&crashed), &bytes).unwrap();
    let t = std::time::Instant::now();
    let db = Db::open(&crashed).expect("a torn tail must open");
    assert!(t.elapsed() < std::time::Duration::from_secs(20), "open took {:?}", t.elapsed());
    assert_eq!(db.get(MAIN, "u4").unwrap().as_deref(), Some("unsynced"), "whole records before the tear stay");
    drop(db);
    cleanup(&[&dir, &crashed]);
}

#[test]
fn settings_entries_that_are_not_numbers_survive_a_write() {
    let dir = tmp("settings");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("settings"), r#"{"note":"keep me","ratio":1.5,"statement_timeout":5}"#).unwrap();
    let db = Db::open(&dir).unwrap();
    db.set_setting(Setting::MaxWorlds, 3).unwrap();
    drop(db);
    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(dir.join("settings")).unwrap()).unwrap();
    assert_eq!(m["note"], "keep me");
    assert_eq!(m["ratio"], 1.5);
    assert_eq!(m["statement_timeout"], 5);
    assert_eq!(m["max_worlds"], 3);
    cleanup(&[&dir]);
}

/// A takeover sets the folder's own database aside and opens the remote's, whose log starts
/// anew: that's not a rollback, so the anchor follows it.
#[cfg(feature = "s3")]
#[test]
fn a_takeover_opens_under_an_anchor() {
    let (a, b, bucket) = (tmp("take-a"), tmp("take-b"), tmp("take-bucket"));
    let url = format!("file://{}", bucket.display());
    let with = |dir: &Path, takeover| Open { remote: Some(url.clone()), takeover, ..anchored(dir) };
    {
        let db = Db::open_with(&a, &with(&a, false)).unwrap();
        for i in 0..200 {
            db.put(MAIN, &format!("k{i}"), "a").unwrap(); // a's log goes well past b's
        }
    }
    {
        let db = Db::open_remote(&b, &url).unwrap();
        db.put(MAIN, "from-b", "1").unwrap();
    }
    let db = Db::open_with(&a, &with(&a, true)).expect("a takeover isn't a rollback");
    assert_eq!(db.get(MAIN, "from-b").unwrap().as_deref(), Some("1"));
    drop(db);
    let db = Db::open_with(&a, &with(&a, false)).expect("and the anchor follows it");
    drop(db);
    cleanup(&[&a, &b, &bucket]);
}
