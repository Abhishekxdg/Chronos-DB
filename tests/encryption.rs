//! Encryption at rest: no file of an encrypted database (pages, log, manifests, history, backups)
//! holds its data in the clear; the right key opens it and nothing else does; tampering is found;
//! crashes, time travel and backups work as without encryption.

use chronos::{Db, MAIN};
use std::fs;
use std::path::{Path, PathBuf};

const SECRET: &str = "sixty-two-zebras-in-a-vault";
const KEY: [u8; 32] = [7; 32];

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-enc-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

/// Every file under `dir` whose bytes contain `needle`.
fn holding(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(holding(&p, needle));
        } else if fs::read(&p).unwrap().windows(needle.len()).any(|w| w == needle) {
            out.push(p);
        }
    }
    out
}

fn q(db: &Db, world: &str, sql: &str) -> Vec<String> {
    let out = chronos::sql::run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    out.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")).collect()
}

/// Rows in pages (checkpointed), in the log (after it), in a world, and in history.
fn fill(db: &Db) -> u64 {
    q(db, MAIN, "create table t (id int primary key, secret text)");
    let rows: Vec<String> = (1..=500).map(|i| format!("({i}, '{SECRET} {i}')")).collect();
    q(db, MAIN, &format!("insert into t values {}", rows.join(",")));
    db.checkpoint().unwrap();
    let before = chronos::sql::moment("now").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    q(db, MAIN, &format!("update t set secret = '{SECRET} changed' where id <= 10"));
    db.fork(MAIN, "w").unwrap();
    db.put("w", "notes/1", &format!("{{\"text\": \"{SECRET} in a world\"}}")).unwrap();
    before
}

#[test]
fn nothing_in_the_clear_and_only_the_key_opens_it() {
    // without a key, the secret is plainly there: the check below means something
    let plain = tmp("plain");
    fill(&Db::open(&plain).unwrap());
    assert!(!holding(&plain, SECRET.as_bytes()).is_empty());

    let dir = tmp("enc");
    let before = {
        let db = Db::open_encrypted(&dir, &KEY).unwrap();
        let before = fill(&db);
        db.checkpoint().unwrap(); // and history
        q(&db, MAIN, &format!("insert into t values (9999, '{SECRET} after')"));
        before
    };
    assert_eq!(holding(&dir, SECRET.as_bytes()), Vec::<PathBuf>::new(), "files holding the secret in the clear");
    assert_eq!(holding(&dir, b"_sonos_tables"), Vec::<PathBuf>::new(), "nor table names");

    let e = |r: std::io::Result<Db>| r.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(e(Db::open(&dir)).contains("is encrypted"), "no key");
    assert!(e(Db::open_encrypted(&dir, &[8; 32])).contains("wrong key"));
    assert!(e(Db::open_encrypted(&plain, &KEY)).contains("created without encryption"));

    let db = Db::open_encrypted(&dir, &KEY).unwrap();
    assert_eq!(q(&db, MAIN, "select count(*) from t"), ["501"]);
    assert_eq!(q(&db, MAIN, &format!("select count(*) from t where secret = '{SECRET} changed'")), ["10"]);
    assert!(db.get("w", "notes/1").unwrap().unwrap().contains("in a world"));
    // time travel reads encrypted history
    db.fork_at(MAIN, before, "then", serde_json::Value::Null).unwrap();
    assert_eq!(q(&db, "then", &format!("select count(*) from t where secret = '{SECRET} changed'")), ["0"]);
    db.verify().unwrap();

    // backups stay encrypted, and restore with the key
    let (to, restored) = (tmp("backup"), tmp("restored"));
    db.backup(&to).unwrap();
    drop(db);
    assert_eq!(holding(&to, SECRET.as_bytes()), Vec::<PathBuf>::new());
    assert!(Db::from_backup(&to, &restored, None).is_err(), "not without the key");
    let _ = fs::remove_dir_all(&restored);
    let db = Db::from_backup(&to, &restored, Some(&KEY)).unwrap();
    assert_eq!(q(&db, MAIN, "select count(*) from t"), ["501"]);
    drop(db);
    for d in [plain, dir, to, restored] {
        let _ = fs::remove_dir_all(d);
    }
}

fn last_segment(dir: &Path) -> PathBuf {
    let mut segs: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().contains("wal-"))
        .collect();
    segs.sort();
    segs.pop().unwrap()
}

#[test]
fn tampering_is_found_and_torn_tails_recover() {
    let dir = tmp("tamper");
    {
        let db = Db::open_encrypted(&dir, &KEY).unwrap();
        for i in 0..50 {
            db.put(MAIN, &format!("k/{i:03}"), &format!("{{\"v\": {i}}}")).unwrap();
        }
    }
    let seg = last_segment(&dir);
    let bytes = fs::read(&seg).unwrap();

    // a torn tail after a crash: cut anywhere, the rows whose records survived come back
    for cut in (0..bytes.len()).step_by(97) {
        let crash = tmp("tamper-crash");
        fs::create_dir_all(&crash).unwrap();
        for e in fs::read_dir(&dir).unwrap().flatten().filter(|e| e.path().is_file()) {
            fs::copy(e.path(), crash.join(e.file_name())).unwrap();
        }
        fs::write(crash.join(seg.file_name().unwrap()), &bytes[..cut]).unwrap();
        let db = Db::open_encrypted(&crash, &KEY).unwrap_or_else(|e| panic!("cut {cut}: {e}"));
        let n = db.scan(MAIN, "k/", usize::MAX).unwrap().len();
        let got: Vec<String> = db.scan(MAIN, "k/", usize::MAX).unwrap().into_iter().map(|(k, _)| k).collect();
        let want: Vec<String> = (0..n).map(|i| format!("k/{i:03}")).collect();
        assert_eq!(got, want, "cut {cut}: a prefix of the writes");
        drop(db);
        let _ = fs::remove_dir_all(&crash);
    }

    // a record changed on disk, with its checksum fixed up to match: refused, not replayed
    let mut t = bytes.clone();
    let len = u32::from_le_bytes(t[..4].try_into().unwrap()) as usize;
    t[8 + len / 2] ^= 1;
    let crc = crc32fast::hash(&t[8..8 + len]);
    t[4..8].copy_from_slice(&crc.to_le_bytes());
    fs::write(&seg, t).unwrap();
    let e = Db::open_encrypted(&dir, &KEY).err().expect("tampered log must not open").to_string();
    assert!(e.contains("doesn't decrypt"), "{e}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_cli_takes_the_key_from_the_environment() {
    let dir = tmp("cli");
    let chronos = |key: Option<&str>, args: &[&str]| {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_chronos"));
        c.arg(&dir).args(args).env_remove("CHRONOS_KEY").env_remove("CHRONOS_KEY_FILE");
        if let Some(k) = key {
            c.env("CHRONOS_KEY", k);
        }
        let o = c.output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    let key = String::from_utf8(
        std::process::Command::new(env!("CARGO_BIN_EXE_chronos")).arg("keygen").output().unwrap().stdout,
    )
    .unwrap();
    let key = key.trim();
    assert_eq!(key.len(), 64);
    assert!(chronos(Some(key), &["put", "users", "1", &format!("{{\"name\": \"{SECRET}\"}}")]).0);
    let (ok, out) = chronos(None, &["get", "users", "1"]);
    assert!(!ok && out.contains("encrypted"), "{out}");
    let (ok, out) = chronos(Some(key), &["get", "users", "1"]);
    assert!(ok && out.contains(SECRET), "{out}");
    assert!(holding(&dir, SECRET.as_bytes()).is_empty());
    let _ = fs::remove_dir_all(&dir);
}
