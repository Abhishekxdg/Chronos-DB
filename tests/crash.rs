//! Crashes in the middle of checkpoints, cleanup and schema changes.
//!
//! Checkpoints and cleanup: a child process runs them with CHRONOS_CRASH_AT=n and aborts at the
//! n-th crash point (see `disk::crash_point`: every step between their file writes, renames and
//! deletions), for every n until one finishes. After each crash the folder must open, hold exactly
//! what it held before, pass `verify`, and checkpoint and clean up normally from there.
//!
//! Schema changes: every statement must be all or nothing. The log is cut at every record
//! boundary and at random bytes (with torn-write garbage), and the database must come back as it
//! was after some whole number of statements, never halfway through a CREATE, ALTER or DROP.

use chronos::{Db, MAIN};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-crash-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
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

/// Every world's rows.
fn fingerprint(db: &Db) -> String {
    let mut out = String::new();
    for (b, parent) in db.branches() {
        // (the planner's statistics are written after the statement that changed them, counted outside its lock, as
        // Postgres's index_update_stats updates pg_class in place: a crash between may leave them a step behind)
        let rows: Vec<_> = db
            .scan(&b, "", usize::MAX)
            .unwrap()
            .into_iter()
            .filter(|(k, _)| !k.starts_with("_sonos_relstats/"))
            .collect();
        out += &format!("{b} <- {parent:?}\n  {rows:?}\n");
    }
    out
}

fn put_all(db: &Db, b: &str, tag: &str) {
    let rows: Vec<(String, String)> =
        (0..1500).map(|i| (format!("t/{i:05}"), format!("{{\"v\": \"{tag}-{i}\"}}"))).collect();
    let rows: Vec<(&str, Option<&str>)> = rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect();
    db.batch(b, &rows).unwrap();
}

const CHILD: &str = "CHRONOS_CRASH_CHILD";

/// The child process of `crash_during_checkpoint_and_cleanup` (a no-op otherwise): checkpoints,
/// cleans up and closes, which changes no row.
#[test]
fn child() {
    let Ok(dir) = std::env::var(CHILD) else { return };
    let db = Db::open(&dir).unwrap();
    db.checkpoint().unwrap();
    db.gc().unwrap();
    db.checkpoint().unwrap();
    db.gc().unwrap();
}

#[test]
fn crash_during_checkpoint_and_cleanup() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: crash points are compiled into debug builds only");
        return;
    }
    let (base, dir) = (tmp("base"), tmp("dir"));
    {
        // pages that cleanup can free: main's first version, and worlds since discarded
        let db = Db::open(&base).unwrap();
        db.set_retention(0).unwrap(); // no history holds old pages
        put_all(&db, MAIN, "one");
        db.checkpoint().unwrap();
        for w in ["w1", "w2", "w3", "w4"] {
            db.fork(MAIN, w).unwrap();
            put_all(&db, w, w);
        }
        db.checkpoint().unwrap();
        for w in ["w1", "w2", "w3"] {
            db.discard(w).unwrap();
        }
        put_all(&db, MAIN, "two");
        db.put("w4", "t/extra", "{}").unwrap();
    } // closing checkpoints
    let want = fingerprint(&Db::open(&base).unwrap());

    let exe = std::env::current_exe().unwrap();
    let mut n = 0;
    loop {
        n += 1;
        assert!(n < 5000, "the child never finished");
        let _ = fs::remove_dir_all(&dir);
        copy_dir(&base, &dir);
        let out = Command::new(&exe)
            .args(["--exact", "child", "--nocapture", "--test-threads=1"])
            .env(CHILD, &dir)
            .env("CHRONOS_CRASH_AT", n.to_string())
            .output()
            .unwrap();
        let finished = out.status.success();
        let db = Db::open(&dir).unwrap_or_else(|e| panic!("crash point {n}: reopening fails: {e}"));
        assert_eq!(fingerprint(&db), want, "crash point {n}: rows changed");
        db.verify().unwrap_or_else(|e| panic!("crash point {n}: {e}"));
        db.checkpoint().unwrap();
        db.gc().unwrap();
        db.verify().unwrap_or_else(|e| panic!("crash point {n}, cleaned up after: {e}"));
        drop(db);
        let db = Db::open(&dir).unwrap();
        assert_eq!(fingerprint(&db), want, "crash point {n}: rows changed after cleaning up");
        drop(db);
        if finished {
            break;
        }
    }
    let pages =
        |d: &Path| fs::read_dir(d.join("pages")).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum::<u64>();
    let (before, after) = (pages(&base), pages(&dir));
    assert!(after < before, "cleanup freed pages ({before} -> {after} bytes), so its deletions were crashed in too");
    eprintln!("{} crash points in checkpoints and cleanup, each recovered", n - 1);
    assert!(n > 20, "only {n} crash points: are they wired in?");
    let _ = fs::remove_dir_all(&base);
    let _ = fs::remove_dir_all(&dir);
}

/// Where a log segment's records end: frames are [u32 len][u32 crc][payload]; the file may go on
/// in zeros written ahead of them (a zero length ends it).
fn log_end(seg: &Path) -> u64 {
    let b = fs::read(seg).unwrap();
    let mut off = 0;
    while let Some(h) = b.get(off..off + 8) {
        let len = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
        if len == 0 || off + 8 + len > b.len() {
            break;
        }
        off += 8 + len;
    }
    off as u64
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

#[test]
fn crash_during_schema_changes() {
    let (dir, crash) = (tmp("schema"), tmp("schema-crash"));
    let rows = |t: &str, n: usize, f: &dyn Fn(usize) -> String| {
        format!("insert into {t} values {}", (1..=n).map(f).collect::<Vec<_>>().join(","))
    };
    let statements = [
        "create table a (id int primary key, name text, n int)".to_string(),
        rows("a", 300, &|i| format!("({i}, 'name {i}', {})", i % 7)),
        "create index a_n on a (n)".into(),
        "alter table a add column tag text default 'none'".into(),
        "create table b (id int primary key, a_id int references a(id) on delete cascade, v text)".into(),
        rows("b", 400, &|i| format!("({i}, {}, 'v{i}')", i % 300 + 1)),
        "create unique index b_v on b (v)".into(),
        "alter table a rename column name to title".into(),
        "delete from a where id <= 30".into(), // cascades into b
        "alter table a alter column n set not null".into(),
        "drop index a_n".into(),
        "alter table a drop column n".into(),
        "update a set tag = 'even' where id % 2 = 0".into(),
        "alter table b add constraint v_set check (v is not null)".into(),
        "alter table a rename to a2".into(),
        "drop table b".into(),
        "create table c (id int primary key, t text unique)".into(),
        rows("c", 50, &|i| format!("({i}, 't{i}')")),
        "drop table c".into(),
    ];
    let seg;
    let mut states = vec![];
    let mut ends = vec![];
    {
        let db = Db::open(&dir).unwrap();
        states.push(fingerprint(&db));
        seg = last_segment(&dir);
        for s in &statements {
            chronos::sql::run(&db, MAIN, s, &[]).unwrap_or_else(|e| panic!("{s}: {e}"));
            // a statement on main is synced before it returns: the log holds all of it by now
            ends.push(log_end(&seg));
            states.push(fingerprint(&db));
        }
    }
    assert_eq!(last_segment(&dir), seg, "no checkpoint in between: one segment holds every statement");
    let len = fs::metadata(&seg).unwrap().len();
    let mut r = 0x9e3779b97f4a7c15u64;
    let mut next = move || {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        r
    };
    let mut cuts: Vec<u64> = ends.iter().flat_map(|&e| [e.saturating_sub(1), e, e + 1]).collect();
    cuts.extend((0..300).map(|_| next() % (len + 1)));
    cuts.extend([0, len]);
    for cut in cuts.into_iter().filter(|&c| c <= len) {
        let _ = fs::remove_dir_all(&crash);
        copy_dir(&dir, &crash);
        let mut bytes = fs::read(&seg).unwrap();
        bytes.truncate(cut as usize);
        if next() % 2 == 0 {
            bytes.extend((0..next() % 40).map(|_| next() as u8)); // torn write garbage
        }
        fs::write(crash.join(seg.file_name().unwrap()), bytes).unwrap();
        let db = Db::open(&crash).unwrap_or_else(|e| panic!("cut {cut}/{len}: {e}"));
        chronos::sql::cleanup(&db);
        let whole = ends.iter().filter(|&&e| e <= cut).count();
        let got = fingerprint(&db);
        // (a cut between a statement's write and its statistics' finds the statement whole: see `fingerprint`)
        let whole = if got != states[whole] && states.get(whole + 1) == Some(&got) { whole + 1 } else { whole };
        assert!(
            got == states[whole],
            "cut {cut}/{len}: not the state after {whole} whole statements (next: {:?})",
            statements.get(whole)
        );
        // and the schema it came back with works
        if whole >= 1 {
            let t = if whole > 14 { "a2" } else { "a" };
            chronos::sql::run(&db, MAIN, &format!("select count(*) from {t}"), &[]).unwrap();
        }
    }
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&crash);
}

const SEQ_CHILD: &str = "CHRONOS_SEQ_CRASH_CHILD";

/// The child process of `sequence_values_are_durable_before_they_are_handed_out` (a no-op
/// otherwise). One thread takes a value of a new sequence, which reserves a block and is held (see
/// CHRONOS_SEQ_STALL_MS) before it makes the reservation durable; another takes the next value from
/// that block and says what it was.
#[test]
fn seq_child() {
    let Ok(dir) = std::env::var(SEQ_CHILD) else { return };
    let db = Db::open(&dir).unwrap();
    std::thread::scope(|s| {
        s.spawn(|| {
            let _ = db.next_values("s", 1);
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let v = db.next_values("s", 1).unwrap()[0];
        println!("HANDED {v}");
        std::thread::sleep(std::time::Duration::from_secs(60));
    });
}

/// A value reaches a client only after the log covers it, whichever caller reserved its block: a
/// kill right after a value was handed out (while the reserving caller is still on its way to the
/// disk) must not have the database give it out again. Postgres logs a sequence ahead and flushes
/// that record before it returns a value past what it logged.
#[test]
fn sequence_values_are_durable_before_they_are_handed_out() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    if !cfg!(debug_assertions) {
        eprintln!("skipped: the stall that makes the race is compiled into debug builds only");
        return;
    }
    let dir = tmp("seq");
    drop(Db::open(&dir).unwrap());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "seq_child", "--nocapture", "--test-threads=1"])
        .env(SEQ_CHILD, &dir)
        .env("CHRONOS_SEQ_STALL_MS", "5000")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut handed = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        // (the harness's "test seq_child ... " comes first on that line)
        if let Some((_, v)) = line.unwrap().split_once("HANDED ") {
            handed = Some(v.trim().parse::<i64>().unwrap());
            break;
        }
    }
    child.kill().unwrap(); // as kill -9: no close, no flush
    child.wait().unwrap();
    let handed = handed.expect("the child never handed out a value");
    let db = Db::open(&dir).unwrap();
    let next = db.next_values("s", 1).unwrap()[0];
    assert!(next > handed, "after a crash the sequence gave {next} again, which a client had been given ({handed})");
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}
