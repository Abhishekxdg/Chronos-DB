//! Backups of a running database: consistent, complete (worlds with unmerged work, history for
//! time travel), checked page by page, and restorable into a new folder. Corruption is found.

use chronos::{Db, MAIN};
use std::path::PathBuf;

fn q(db: &Db, world: &str, sql: &str) -> Vec<String> {
    let out = chronos::sql::run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    out.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")).collect()
}

/// `n` rows: (1, 'v1'), (2, 'v2'), ... with `pad` more characters each.
fn rows(n: usize, pad: usize) -> String {
    let v: Vec<String> = (1..=n).map(|i| format!("({i}, '{}v{i}')", "x".repeat(pad))).collect();
    format!("insert into t values {}", v.join(","))
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-backup-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn backup_while_running_then_restore() {
    let (dir, to, restored) = (tmp("db"), tmp("to"), tmp("restored"));
    let db = Db::open(&dir).unwrap();
    q(&db, MAIN, "create table t (id int primary key, v text)");
    q(&db, MAIN, &rows(2000, 0));
    db.checkpoint().unwrap();
    let before = chronos::sql::moment("now").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    q(&db, MAIN, "update t set v = 'changed' where id <= 10");
    db.fork(MAIN, "w").unwrap();
    q(&db, "w", "delete from t where id > 1990"); // a world's unmerged work, not yet synced

    let b = db.backup(&to).unwrap();
    assert!(b.files >= 3 && b.bytes > 0, "{b:?}");
    q(&db, MAIN, "insert into t values (9999, 'after the backup')"); // not in it
    let want = (q(&db, MAIN, "select count(*), sum(id) from t where id < 9999"), q(&db, "w", "select count(*) from t"));
    assert!(db.backup(&to).unwrap_err().to_string().contains("not empty"));
    drop(db);

    let db = Db::from_backup(&to, &restored, None).unwrap();
    assert_eq!(q(&db, MAIN, "select count(*), sum(id) from t"), want.0);
    assert_eq!(q(&db, "w", "select count(*) from t"), want.1, "the world came back with its work");
    assert_eq!(q(&db, MAIN, "select count(*) from t where v = 'changed'"), ["10"]);
    // history came too: the restored database goes back to before the update
    db.fork_at(MAIN, before, "then", serde_json::Value::Null).unwrap();
    assert_eq!(q(&db, "then", "select count(*) from t where v = 'changed'"), ["0"]);
    db.discard("then").unwrap();
    // merges work, so a world forked before the backup is a normal world
    assert_eq!(db.merge("w", chronos::Resolve::Fail).unwrap(), 10);
    let v = db.verify().unwrap();
    assert!(v.pages > 0 && v.log_records > 0, "{v:?}");
    drop(db);
    for d in [dir, to, restored] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn verify_finds_corruption() {
    let (dir, to) = (tmp("corrupt"), tmp("corrupt-to"));
    let db = Db::open(&dir).unwrap();
    q(&db, MAIN, "create table t (id int primary key, v text)");
    q(&db, MAIN, &rows(3000, 100));
    db.backup(&to).unwrap();
    drop(db);
    assert!(Db::open(&to).unwrap().verify().is_ok());
    // flip a byte in the middle of a pack
    let pack = std::fs::read_dir(to.join("pages"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| !p.to_string_lossy().ends_with(".idx"))
        .max_by_key(|p| std::fs::metadata(p).unwrap().len())
        .unwrap();
    let mut bytes = std::fs::read(&pack).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&pack, bytes).unwrap();
    let e = Db::open(&to).and_then(|db| db.verify()).unwrap_err().to_string();
    assert!(e.contains("corrupt"), "{e}");
    for d in [dir, to] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Writers keep going during a backup, and the backup is a clean cut: every write before some
/// moment, none after.
#[test]
fn backup_under_writes_is_a_clean_cut() {
    let (dir, to, restored) = (tmp("busy"), tmp("busy-to"), tmp("busy-restored"));
    let db = Db::open(&dir).unwrap();
    q(&db, MAIN, "create table t (id int primary key)");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let done = std::sync::atomic::AtomicUsize::new(0);
    // waits for the writer to get this far: on a busy machine (CI runs every test at once) a sleep alone
    // has seen it write nothing before the backup
    let wrote = |n: usize| {
        while done.load(std::sync::atomic::Ordering::Relaxed) < n {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) || i < 500 {
                i += 1;
                q(&db, MAIN, &format!("insert into t values ({i})"));
                done.store(i, std::sync::atomic::Ordering::Relaxed);
                if i % 200 == 0 {
                    db.put("w", &format!("k/{i}"), "{}").ok(); // a world written unsynced, when it exists
                }
            }
        });
        wrote(50);
        db.fork(MAIN, "w").unwrap();
        wrote(250); // a world write (every 200th) after the fork
        db.backup(&to).unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    drop(db);
    let db = Db::from_backup(&to, &restored, None).unwrap();
    let got = q(&db, MAIN, "select count(*), coalesce(max(id), 0) from t");
    let (n, max) = got[0].split_once('|').unwrap();
    assert!(n == max && n != "0", "ids 1..{max} with none missing: {n} rows");
    drop(db);
    for d in [dir, to, restored] {
        let _ = std::fs::remove_dir_all(d);
    }
}
