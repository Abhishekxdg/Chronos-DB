//! Rollback detection: with an anchor (a file outside the folder), a folder whose files were put
//! back to an earlier point refuses to open, a normal reopen and a crash before the anchor moved
//! still open, the reset accepts and re-anchors, a missing anchor is refused, and (encrypted) an
//! anchor tampered with is refused.

use chronos::{Db, MAIN, Open};
use std::fs;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-review2-anchor-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    let _ = fs::remove_file(anchor_of(&d));
    d
}

fn anchor_of(dir: &Path) -> PathBuf {
    dir.with_extension("anchor")
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

fn opts(dir: &Path) -> Open {
    Open { anchor: Some(anchor_of(dir)), ..Default::default() }
}

fn open(dir: &Path) -> std::io::Result<Db> {
    Db::open_with(dir, &opts(dir))
}

fn err(r: std::io::Result<Db>) -> String {
    match r {
        Ok(_) => panic!("opened; expected a refusal"),
        Err(e) => e.to_string(),
    }
}

fn cleanup(dirs: &[&Path]) {
    for d in dirs {
        let _ = fs::remove_dir_all(d);
        let _ = fs::remove_file(anchor_of(d));
    }
}

#[test]
fn a_folder_copied_back_from_earlier_is_refused() {
    let (dir, old) = (tmp("back"), tmp("back-old"));
    let db = open(&dir).unwrap();
    db.put(MAIN, "t/1", "one").unwrap();
    drop(db);
    copy_dir(&dir, &old); // the earlier point
    let db = open(&dir).unwrap();
    db.put(MAIN, "t/2", "two").unwrap(); // only in the log: the close anchors its position
    drop(db);
    let db = open(&dir).unwrap();
    db.put(MAIN, "t/3", "three").unwrap();
    db.checkpoint().unwrap(); // and after a checkpoint
    drop(db);

    fs::remove_dir_all(&dir).unwrap();
    copy_dir(&old, &dir); // put the older, consistent files back
    let e = err(open(&dir));
    assert!(e.contains("older than its anchor") && e.contains("XX0A1"), "{e}");
    cleanup(&[&dir, &old]);
}

#[test]
fn a_reopen_and_a_crash_before_the_anchor_moved_both_open() {
    let (dir, crashed) = (tmp("crash"), tmp("crash-copy"));
    let db = open(&dir).unwrap();
    db.put(MAIN, "t/1", "one").unwrap();
    drop(db);
    let db = open(&dir).unwrap(); // a normal reopen
    assert!(db.get(MAIN, "t/1").unwrap().is_some());
    db.put(MAIN, "t/2", "two").unwrap();
    let before = fs::read(anchor_of(&dir)).unwrap();
    db.checkpoint().unwrap();
    db.put(MAIN, "t/3", "three").unwrap(); // synced: in the log after the manifest
    // "kill -9" after the manifest was published but before the anchor moved: the folder as it is
    // now, with the anchor as it was
    copy_dir(&dir, &crashed);
    fs::write(anchor_of(&crashed), &before).unwrap();
    drop(db);

    let db = open(&crashed).unwrap(); // ahead of its anchor: fine
    assert!(db.get(MAIN, "t/3").unwrap().is_some());
    drop(db);
    assert_ne!(fs::read(anchor_of(&crashed)).unwrap(), before, "the anchor moved on");
    let db = open(&crashed).unwrap();
    drop(db);
    let db = open(&dir).unwrap(); // and the original, closed cleanly
    assert!(db.get(MAIN, "t/3").unwrap().is_some());
    drop(db);
    cleanup(&[&dir, &crashed]);
}

#[test]
fn the_reset_accepts_and_re_anchors() {
    let (dir, old) = (tmp("reset"), tmp("reset-old"));
    let db = open(&dir).unwrap();
    db.put(MAIN, "t/1", "one").unwrap();
    drop(db);
    copy_dir(&dir, &old);
    let db = open(&dir).unwrap();
    db.put(MAIN, "t/2", "two").unwrap();
    db.checkpoint().unwrap();
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
    copy_dir(&old, &dir); // a deliberate restore of the earlier state
    assert!(err(open(&dir)).contains("older than its anchor"));

    let db = Db::open_with(&dir, &Open { anchor_reset: true, ..opts(&dir) }).unwrap();
    assert!(db.get(MAIN, "t/1").unwrap().is_some());
    assert!(db.get(MAIN, "t/2").unwrap().is_none(), "the restored state, as it was");
    drop(db);
    let db = open(&dir).unwrap(); // re-anchored: opens normally from here
    drop(db);

    // another database's anchor is refused, unless reset
    let other = tmp("reset-other");
    drop(Db::open(&other).unwrap());
    let e = err(Db::open_with(&other, &opts(&dir)));
    assert!(e.contains("another database"), "{e}");
    cleanup(&[&dir, &old, &other]);
}

#[test]
fn a_missing_anchor_is_refused() {
    let dir = tmp("missing");
    drop(open(&dir).unwrap()); // a new folder: the first open makes its anchor
    assert!(anchor_of(&dir).exists());
    fs::remove_file(anchor_of(&dir)).unwrap();
    let e = err(open(&dir));
    assert!(e.contains("missing"), "{e}");

    // an existing database starts being anchored only when asked to
    let plain = tmp("missing-plain");
    let db = Db::open(&plain).unwrap();
    db.put(MAIN, "t/1", "one").unwrap();
    drop(db);
    assert!(err(open(&plain)).contains("missing"));
    drop(Db::open_with(&plain, &Open { anchor_reset: true, ..opts(&plain) }).unwrap());
    drop(open(&plain).unwrap());
    cleanup(&[&dir, &plain]);
}

#[test]
fn an_encrypted_databases_anchor_tampered_with_is_refused() {
    let dir = tmp("enc");
    let o = Open { key: Some([7; 32]), ..opts(&dir) };
    let db = Db::open_with(&dir, &o).unwrap();
    db.put(MAIN, "t/1", "one").unwrap();
    drop(db);
    let good = fs::read(anchor_of(&dir)).unwrap();
    assert!(good.starts_with(b"SONOSENC"), "sealed with the key");
    drop(Db::open_with(&dir, &o).unwrap());

    let mut bad = fs::read(anchor_of(&dir)).unwrap();
    let n = bad.len();
    bad[n - 3] ^= 1;
    fs::write(anchor_of(&dir), &bad).unwrap();
    let e = err(Db::open_with(&dir, &o));
    assert!(e.contains("doesn't decrypt"), "{e}");
    // nor can a plain one be put in its place without the key
    fs::write(anchor_of(&dir), b"SONOSANC1").unwrap();
    assert!(err(Db::open_with(&dir, &o)).contains("isn't encrypted"));
    cleanup(&[&dir]);
}
