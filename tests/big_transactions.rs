//! Big writes inside a transaction (BEGIN ... COMMIT, and CREATE TABLE AS, which makes one of its own; `pg_restore -1`,
//! `psql --single-transaction` and most migration tools load data so). A COPY, INSERT, UPDATE or DELETE too big to
//! hold goes on in parts that spill to disk as it does outside a transaction (see `sql::bulk`), into the transaction's
//! world, and COMMIT makes the transaction's state its world's at once (see `Core::merge_published`) instead of
//! merging its rows one by one in memory. This file's process runs with a 256 kB work budget, so a few thousand rows
//! go in parts; it checks that the transaction sees its own spilled rows (scans, index lookups, unique and foreign-key
//! checks), that ROLLBACK and ROLLBACK TO SAVEPOINT drop them, that others see nothing before COMMIT and everything
//! after, that READ COMMITTED's view of what others commit meanwhile holds, that a kill before COMMIT leaves nothing
//! and one after it everything, and that memory doesn't grow with the transaction.
//!
//! `big_transaction_peak_rss` (ignored) is the measurement on a real size: run it alone, one size at a time, as
//! `CHRONOS_TEST_ROWS=5000000 CHRONOS_TEST_MODE=copy cargo test --release --test big_transactions -- --ignored --exact
//! big_transaction_peak_rss --nocapture` (under `systemd-run --user --scope -p MemoryMax=30G` on the VM); it prints the
//! process's peak RSS. Modes: `insert` (INSERT ... SELECT), `copy`, `ctas` (CREATE TABLE AS, up to 10 million rows),
//! `update`, and `alone`, the same INSERTs outside a transaction.

use chronos::sql::{Session, parse};
use chronos::{Db, MAIN};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Counting;
static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            PEAK.fetch_max(NOW.fetch_add(l.size(), Relaxed) + l.size(), Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        NOW.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, size) };
        if !q.is_null() {
            NOW.fetch_sub(l.size(), Relaxed);
            PEAK.fetch_max(NOW.fetch_add(size, Relaxed) + size, Relaxed);
        }
        q
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

static SERIAL: Mutex<()> = Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
    // set before anything reads them (each is read once per process)
    unsafe {
        std::env::set_var("CHRONOS_WORK_MEM", "256kB");
        std::env::set_var("CHRONOS_CACHE_MB", "8");
    }
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-bigtx-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn q(db: &Db, s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    let o = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    o.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect()).collect()
}

fn one(db: &Db, s: &mut Session, sql: &str) -> String {
    q(db, s, sql)[0][0].clone()
}

/// The error a statement fails with: its SQLSTATE and message.
fn fails(db: &Db, s: &mut Session, sql: &str) -> (String, String) {
    match s.run(db, sql, &[]) {
        Ok(_) => panic!("{sql}: should fail"),
        Err(e) => (e.code.to_string(), e.message),
    }
}

/// COPY `t` FROM STDIN (csv) with `data`, handed over 64 kB at a time as a client sends it.
fn copy_in(db: &Db, s: &mut Session, t: &str, data: Vec<u8>) -> Result<String, (String, String)> {
    let c = s
        .copy_in(db, &parse(&format!("copy {t} from stdin csv")).unwrap().pop().unwrap())
        .map_err(|e| (e.code.to_string(), e.message.clone()))?;
    let mut at = 0;
    let next = move || {
        let piece = data.get(at..(at + 65_536).min(data.len())).filter(|p| !p.is_empty()).map(<[u8]>::to_vec);
        at += 65_536;
        Ok(piece)
    };
    c.run(s, db, Box::new(next)).map(|o| o.tag).map_err(|e| (e.code.to_string(), e.message))
}

/// Rows `from..from + n` of a table made by `table`, as CSV.
fn rows(from: usize, n: usize) -> Vec<u8> {
    (from..from + n).map(|i| format!("{i},{},u{i},{}\n", i % 97, "b".repeat(200))).collect::<String>().into_bytes()
}

const ROW: &str = "i, i % 97, 'u' || i, repeat('b', 200)";

fn table(db: &Db, s: &mut Session, t: &str) {
    q(db, s, &format!("create table {t} (id int primary key, g int, u text unique, body text)"));
    q(db, s, &format!("create index {t}_g on {t} (g)"));
}

fn spilled(db: &Db) -> u64 {
    db.counters().into_iter().find(|(n, _)| *n == "spilled_bytes").map_or(0, |(_, v)| v)
}

fn spill_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir.join("spill")).map_or(true, |mut d| d.next().is_none())
}

/// Every row of main under `prefix`, without it.
fn keys(db: &Db, prefix: &str) -> Vec<(String, String)> {
    db.prefix(MAIN, prefix).unwrap().into_iter().map(|(k, v)| (k[prefix.len()..].to_string(), v.to_string())).collect()
}

/// A table's rows, index entries and unique rows (as `t`'s, whatever its name).
fn stored(db: &Db, t: &str) -> [Vec<(String, String)>; 3] {
    [
        keys(db, &format!("{t}/")),
        keys(db, &format!("_sonos_idx/{t}/{t}_g/")),
        keys(db, &format!("_sonos_uniq/{t}/{t}_u_key/")),
    ]
}

#[test]
fn a_transaction_sees_its_own_big_writes_and_commits_them_whole() {
    let _one = setup();
    let dir = tmp("own");
    let db = Db::open(&dir).unwrap();
    let (mut s, mut other) = (Session::new(MAIN), Session::new(MAIN));
    table(&db, &mut s, "t");
    table(&db, &mut s, "s");
    // the same rows, 100 a statement outside a transaction: each held in memory
    for at in (0..12_000).step_by(100) {
        q(&db, &mut s, &format!("insert into s select {ROW} from generate_series({}, {}) i", at + 1, at + 100));
    }
    let before = spilled(&db);
    q(&db, &mut s, "begin");
    q(&db, &mut s, &format!("insert into t select {ROW} from generate_series(1, 6000) i"));
    assert!(spilled(&db) > before, "6,000 rows of 200 bytes don't fit in 256 kB: it spilled");
    assert_eq!(copy_in(&db, &mut s, "t", rows(6001, 6000)).unwrap(), "COPY 6000");
    // the transaction reads its spilled rows: by scan, by key, by index, by unique value
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "12000");
    assert_eq!(
        q(&db, &mut s, "select id, g from t where id between 4000 and 4002 order by id"),
        [["4000", "23"], ["4001", "24"], ["4002", "25"]]
    );
    assert_eq!(one(&db, &mut s, "select count(*) from t where g = 5"), "124");
    assert_eq!(one(&db, &mut s, "select id from t where u = 'u9321'"), "9321");
    // and so do its triggers
    q(&db, &mut s, "create table x (n bigint)");
    q(
        &db,
        &mut s,
        "create function seen() returns trigger language plpgsql as $$ begin new.n := (select count(*) from t where g = 5); \
         return new; end $$",
    );
    q(&db, &mut s, "create trigger x_seen before insert on x for each row execute function seen()");
    q(&db, &mut s, "insert into x values (0)");
    assert_eq!(one(&db, &mut s, "select n from x"), "124");
    // nobody else sees any of it
    assert_eq!(one(&db, &mut other, "select count(*) from t"), "0");
    // its unique checks meet its spilled rows (a savepoint keeps the transaction going)
    q(&db, &mut s, "savepoint a");
    assert_eq!(fails(&db, &mut s, "insert into t values (20000, 1, 'u4321', 'x')").0, "23505");
    q(&db, &mut s, "rollback to savepoint a");
    assert_eq!(fails(&db, &mut s, "insert into t values (4321, 1, 'new', 'x')").0, "23505");
    q(&db, &mut s, "rollback to savepoint a");
    // and an UPDATE and a DELETE of them, in parts too
    q(&db, &mut s, "update t set body = upper(body) where id <= 6000");
    q(&db, &mut s, "update t set body = lower(body) where id <= 6000");
    q(&db, &mut s, "delete from t where id > 12000");
    q(&db, &mut s, "commit");
    assert_eq!(one(&db, &mut other, "select count(*) from t"), "12000");
    let (a, b) = (stored(&db, "t"), stored(&db, "s"));
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.len(), 12_000);
        assert!(x == y, "the same rows and entries as small statements outside a transaction leave");
    }
    for sql in ["select count(*), sum(g), max(u), min(body) from t", "select id from t where u = 'u4321'"] {
        assert_eq!(q(&db, &mut other, sql), q(&db, &mut other, &sql.replace(" t", " s")), "{sql}");
    }
    db.verify().unwrap();
    assert!(spill_empty(&dir));
    drop(db);
    // and it's all there after a reopen
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(one(&db, &mut s, "select count(*) from t where g = 5"), "124");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rollback_and_rollback_to_savepoint_drop_big_writes() {
    let _one = setup();
    let dir = tmp("rollback");
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    table(&db, &mut s, "t");
    q(&db, &mut s, "begin");
    copy_in(&db, &mut s, "t", rows(1, 6000)).unwrap();
    q(&db, &mut s, "savepoint a");
    copy_in(&db, &mut s, "t", rows(6001, 6000)).unwrap();
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "12000");
    q(&db, &mut s, "rollback to savepoint a");
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "6000");
    assert_eq!(one(&db, &mut s, "select count(*) from t where u = 'u9000'"), "0", "its unique rows went too");
    // the rows it rolled back can come again: their keys and unique values are free
    q(&db, &mut s, &format!("insert into t select {ROW} from generate_series(6001, 9000) i"));
    q(&db, &mut s, "savepoint b");
    q(&db, &mut s, &format!("insert into t select {ROW} from generate_series(9001, 15000) i"));
    q(&db, &mut s, "release savepoint b");
    q(&db, &mut s, "release savepoint a");
    q(&db, &mut s, "commit");
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "15000");
    assert_eq!(stored(&db, "t").map(|x| x.len()), [15_000; 3]);
    // a whole transaction rolled back
    q(&db, &mut s, "begin");
    q(&db, &mut s, &format!("insert into t select {ROW} from generate_series(15001, 30000) i"));
    q(&db, &mut s, "update t set g = -g");
    assert_eq!(one(&db, &mut s, "select count(*) from t where g <= 0"), "30000");
    q(&db, &mut s, "rollback");
    assert_eq!(one(&db, &mut s, "select count(*) from t where g > 0 or id % 97 = 0"), "15000");
    assert_eq!(stored(&db, "t").map(|x| x.len()), [15_000; 3]);
    db.verify().unwrap();
    assert!(spill_empty(&dir));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_unique_violation_late_in_a_million_row_copy_fails_the_transaction() {
    let _one = setup();
    let dir = tmp("unique");
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    q(&db, &mut s, "create table t (id int primary key, u int unique)");
    q(&db, &mut s, "insert into t select i, i from generate_series(1, 10) i");
    q(&db, &mut s, "begin");
    let data = |from: usize, n: usize, last: usize| -> Vec<u8> {
        let mut d: String = (from..from + n - 1).map(|i| format!("{i},{i}\n")).collect();
        d += &format!("{},{last}\n", from + n - 1);
        d.into_bytes()
    };
    // the million rows' last one takes a unique value the first of them took
    let (code, message) = copy_in(&db, &mut s, "t", data(1001, 1_000_000, 1001)).unwrap_err();
    assert_eq!(code, "23505", "{message}");
    assert_eq!(fails(&db, &mut s, "select 1").0, "25P02", "the transaction failed");
    q(&db, &mut s, "rollback");
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "10");
    // a million rows in, then a second COPY whose last row takes a value one of them took
    q(&db, &mut s, "begin");
    assert_eq!(copy_in(&db, &mut s, "t", data(1001, 1_000_000, 1_001_000)).unwrap(), "COPY 1000000");
    q(&db, &mut s, "savepoint a");
    let (code, _) = copy_in(&db, &mut s, "t", data(2_000_001, 5000, 777_777)).unwrap_err();
    assert_eq!(code, "23505");
    q(&db, &mut s, "rollback to savepoint a");
    copy_in(&db, &mut s, "t", data(2_000_001, 5000, 0)).unwrap();
    q(&db, &mut s, "commit");
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "1005010");
    assert_eq!(one(&db, &mut s, "select id from t where u = 777777"), "777777");
    db.verify().unwrap();
    assert!(spill_empty(&dir));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn foreign_keys_hold_across_big_writes_in_a_transaction() {
    let _one = setup();
    let dir = tmp("fk");
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    q(&db, &mut s, "create table p (id int primary key, body text)");
    q(&db, &mut s, "create table c (id int primary key, p int references p, body text)");
    q(&db, &mut s, "begin");
    q(&db, &mut s, "insert into p select i, repeat('p', 200) from generate_series(1, 6000) i");
    // children of the parents just written, in parts
    q(&db, &mut s, "insert into c select i, i, repeat('c', 200) from generate_series(1, 6000) i");
    q(&db, &mut s, "savepoint a");
    // the last of them has no parent
    let (code, message) =
        fails(&db, &mut s, "insert into c select i, i - 6000, repeat('c', 200) from generate_series(6001, 12001) i");
    assert_eq!(code, "23503", "{message}");
    q(&db, &mut s, "rollback to savepoint a");
    // a parent its children still reference
    assert_eq!(fails(&db, &mut s, "delete from p where id = 4321").0, "23503");
    q(&db, &mut s, "rollback to savepoint a");
    q(&db, &mut s, "commit");
    assert_eq!(one(&db, &mut s, "select count(*) from c join p on p.id = c.p"), "6000");
    assert_eq!(fails(&db, &mut s, "delete from p where id = 17").0, "23503");
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What other sessions commit while a big transaction runs: a READ COMMITTED statement reads it, and COMMIT keeps it,
/// with the transaction's own rows; a unique value both took fails the commit, and leaves nothing of it.
#[test]
fn others_commits_meanwhile_stay_and_clash_as_they_did() {
    let _one = setup();
    let dir = tmp("rc");
    let db = Db::open(&dir).unwrap();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    table(&db, &mut a, "t");
    q(&db, &mut a, "create table o (id int primary key, note text)");
    q(&db, &mut a, "begin");
    q(&db, &mut a, &format!("insert into t select {ROW} from generate_series(1, 6000) i"));
    // another session's commits, to another table and to this one
    q(&db, &mut b, "insert into o values (1, 'meanwhile')");
    q(&db, &mut b, "insert into t values (70000, 1, 'b70000', 'theirs')");
    // the next statement reads them, with its own rows
    assert_eq!(one(&db, &mut a, "select count(*) from t"), "6001");
    q(&db, &mut a, &format!("insert into t select {ROW} from generate_series(6001, 12000) i"));
    q(&db, &mut b, "insert into o values (2, 'just before the commit')");
    q(&db, &mut a, "commit");
    assert_eq!(one(&db, &mut b, "select count(*) from t"), "12001");
    assert_eq!(one(&db, &mut b, "select count(*) from o"), "2");
    assert_eq!(one(&db, &mut b, "select count(*) from t where g = 1"), "125");
    assert_eq!(stored(&db, "t").map(|x| x.len()), [12_001; 3]);
    // a unique value the other session committed meanwhile
    q(&db, &mut a, "begin");
    q(&db, &mut a, &format!("insert into t select {ROW} from generate_series(20001, 26000) i"));
    q(&db, &mut b, "insert into t values (80000, 1, 'u20017', 'theirs')");
    let (code, message) = fails(&db, &mut a, "commit");
    assert_eq!(code, "23505", "{message}");
    assert_eq!(one(&db, &mut b, "select count(*) from t"), "12002");
    assert_eq!(one(&db, &mut b, "select id from t where u = 'u20017'"), "80000");
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn memory_doesnt_grow_with_the_transaction() {
    let _one = setup();
    let dir = tmp("memory");
    let db = Db::open(&dir).unwrap();
    let (mut s, mut other) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut s, "create table o (id int primary key, note text)");
    // first fills the page cache (CHRONOS_CACHE_MB), which every measured transaction then starts with
    table(&db, &mut s, "warm");
    q(&db, &mut s, &format!("insert into warm select {ROW} from generate_series(1, 80000) i"));
    let measure = |f: &mut dyn FnMut()| {
        let base = NOW.load(Relaxed);
        PEAK.store(base, Relaxed);
        f();
        (PEAK.load(Relaxed) - base, NOW.load(Relaxed).saturating_sub(base))
    };
    let flat = |what: &str, grew: [(usize, usize); 2]| {
        let [(a, held_a), (b, held_b)] = grew;
        eprintln!(
            "{what}: heap growth 20,000 rows {} kB, 80,000 rows {} kB; held after COMMIT {} kB, {} kB",
            a >> 10,
            b >> 10,
            held_a >> 10,
            held_b >> 10
        );
        // (merged row by row in memory: 69 MB at 80,000 rows for an INSERT, and 54 MB of it left after COMMIT;
        // CREATE TABLE AS's own statement, into a table without a key, grows a little, 5 MB here, as it did before:
        // flat at a real size, 378 MB of RSS at 1 and 5 million rows)
        let slack = if what == "create table as" { 8 << 20 } else { 4 << 20 };
        assert!(b < a + slack, "{what} {grew:?}: four times the rows, about the same memory");
        assert!(b < 48 << 20, "{what} {grew:?}");
        assert!(held_b < 8 << 20, "{what} {grew:?}: nothing of it is left in memory after COMMIT");
    };
    let sizes = [20_000usize, 80_000];
    let mut n = 0;
    let mut name = |kind: &str| {
        n += 1;
        format!("{kind}{n}")
    };
    let names: Vec<[String; 2]> = ["i", "c", "a", "u", "m"].iter().map(|k| [name(k), name(k)]).collect();
    for t in names.iter().flatten().filter(|t| !t.starts_with('a')) {
        table(&db, &mut s, t);
    }
    let mut i = 0;
    flat(
        "insert",
        sizes.map(|n| {
            let t = &names[0][i % 2];
            i += 1;
            measure(&mut || {
                q(&db, &mut s, "begin");
                q(&db, &mut s, &format!("insert into {t} select {ROW} from generate_series(1, {n}) i"));
                q(&db, &mut s, "commit");
            })
        }),
    );
    flat(
        "copy",
        sizes.map(|n| {
            let t = &names[1][i % 2];
            i += 1;
            let mut data = Some(rows(1, n)); // made before, handed over inside
            measure(&mut || {
                q(&db, &mut s, "begin");
                copy_in(&db, &mut s, t, data.take().unwrap()).unwrap();
                q(&db, &mut s, "commit");
            })
        }),
    );
    flat(
        "create table as",
        sizes.map(|n| {
            let t = &names[2][i % 2];
            i += 1;
            measure(&mut || {
                q(
                    &db,
                    &mut s,
                    &format!(
                        "create table {t} as select i as id, i % 97 as g, 'u' || i as u, repeat('b', 200) as body \
                         from generate_series(1, {n}) i"
                    ),
                );
            })
        }),
    );
    flat(
        "update",
        sizes.map(|n| {
            let t = &names[3][i % 2];
            i += 1;
            q(&db, &mut s, &format!("insert into {t} select {ROW} from generate_series(1, {n}) i"));
            measure(&mut || {
                q(&db, &mut s, "begin");
                q(&db, &mut s, &format!("update {t} set body = upper(body)"));
                q(&db, &mut s, "commit");
            })
        }),
    );
    // another session commits while it runs: COMMIT brings the transaction up to it, then checks its rows in parts
    flat(
        "insert, with another commit meanwhile",
        sizes.map(|n| {
            let t = &names[4][i % 2];
            i += 1;
            measure(&mut || {
                q(&db, &mut s, "begin");
                q(&db, &mut s, &format!("insert into {t} select {ROW} from generate_series(1, {n}) i"));
                q(&db, &mut other, &format!("insert into o values ({n} + {i}, 'meanwhile')"));
                q(&db, &mut s, "commit");
            })
        }),
    );
    assert_eq!(one(&db, &mut s, "select count(*) from o"), "2");
    for t in names.iter().flatten() {
        assert_eq!(
            one(&db, &mut s, &format!("select count(*) from {t}")),
            if t.ends_with(['1', '3', '5', '7', '9']) { "20000" } else { "80000" },
            "{t}"
        );
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

const CHILD: &str = "CHRONOS_BIGTX_CHILD";

/// The child process of the kill and crash tests (a no-op otherwise): a transaction that loads 60,000 rows in parts and
/// commits, then marks that it committed, checkpoints, marks that too and waits to be killed.
#[test]
fn child() {
    let Ok(dir) = std::env::var(CHILD) else { return };
    unsafe { std::env::set_var("CHRONOS_WORK_MEM", "256kB") };
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    q(&db, &mut s, "begin");
    q(&db, &mut s, &format!("insert into t select {ROW} from generate_series(1001, 31000) i"));
    copy_in(&db, &mut s, "t", rows(31001, 30000)).unwrap();
    q(&db, &mut s, "commit");
    std::fs::write(Path::new(&dir).join("committed"), "").unwrap();
    db.checkpoint().unwrap();
    std::fs::write(Path::new(&dir).join("checkpointed"), "").unwrap();
    std::thread::sleep(std::time::Duration::from_secs(600));
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

/// The folder after a kill: opened as the server opens it (transactions left by dead sessions dropped), it holds the
/// 1,000 rows from before or all 61,000, whole, and all of them once the child said it committed.
fn after_kill(dir: &Path, when: &str) -> bool {
    let committed = dir.join("committed").exists();
    let db = Db::open(dir).unwrap_or_else(|e| panic!("{when}: {e}"));
    chronos::sql::cleanup(&db);
    assert!(spill_empty(dir), "opening empties the spill folder");
    db.verify().unwrap_or_else(|e| panic!("{when}: {e}"));
    let mut s = Session::new(MAIN);
    let n = one(&db, &mut s, "select count(*) from t");
    let lens = stored(&db, "t").map(|x| x.len());
    eprintln!("{when}: {n} rows{}", if committed { ", committed" } else { "" });
    match n.as_str() {
        "1000" if !committed => assert_eq!(lens, [1000; 3], "{when}"),
        "61000" => {
            assert_eq!(lens, [61_000; 3], "{when}");
            assert_eq!(one(&db, &mut s, "select id from t where u = 'u45678'"), "45678");
        }
        _ => panic!("{when}: {n} rows, neither before nor after (committed: {committed})"),
    }
    assert!(db.worlds().iter().all(|w| !w.name.starts_with("_tx_")), "{when}: no transaction left");
    n == "61000"
}

fn base_folder(name: &str) -> PathBuf {
    let base = tmp(name);
    let db = Db::open(&base).unwrap();
    let mut s = Session::new(MAIN);
    table(&db, &mut s, "t");
    q(&db, &mut s, &format!("insert into t select {ROW} from generate_series(1, 1000) i"));
    base
}

#[test]
fn a_kill_before_commit_leaves_nothing_and_after_it_everything() {
    let _one = setup();
    let (base, dir) = (base_folder("kill-base"), tmp("kill"));
    let exe = std::env::current_exe().unwrap();
    let (mut mid, mut ms) = (0, 100);
    loop {
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&base, &dir);
        let mut child = Command::new(&exe)
            .args(["--exact", "child", "--nocapture", "--test-threads=1"])
            .env(CHILD, &dir)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(ms));
        let _ = child.kill(); // SIGKILL
        child.wait().unwrap();
        let spilling = !spill_empty(&dir);
        let done = after_kill(&dir, &format!("killed after {ms} ms{}", if spilling { ", while spilling" } else { "" }));
        mid += (!done) as usize;
        if done && dir.join("committed").exists() {
            break;
        }
        ms += 150;
        assert!(ms < 600_000, "the transaction never committed");
    }
    assert!(mid >= 1, "no kill came before the commit");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// In a debug build, the child aborts at a crash point (`CHRONOS_CRASH_AT`: the steps of the checkpoints that commit
/// each of its statements to the transaction's world, and of the one after its COMMIT), one after another (every one
/// at first, then every few: a run passes some hundreds), and the folder must hold what it held before or, once
/// COMMIT has returned, everything.
#[test]
fn a_crash_at_each_step_before_commit_leaves_nothing() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: crash points are in debug builds only");
        return;
    }
    let _one = setup();
    let (base, dir) = (base_folder("crash-base"), tmp("crash"));
    let exe = std::env::current_exe().unwrap();
    let mut at = 1;
    loop {
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&base, &dir);
        let mut child = Command::new(&exe)
            .args(["--exact", "child", "--nocapture", "--test-threads=1"])
            .env(CHILD, &dir)
            .env("CHRONOS_CRASH_AT", at.to_string())
            .spawn()
            .unwrap();
        // it aborts at the crash point, or commits, checkpoints and waits
        let start = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() && !dir.join("checkpointed").exists() {
            assert!(start.elapsed().as_secs() < 600, "crash point {at}: the child never ended");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        child.wait().unwrap();
        let done = dir.join("checkpointed").exists();
        after_kill(&dir, &format!("crash point {at}"));
        if done {
            break;
        }
        at += 1 + at / 8;
    }
    assert!(at > 1, "the child passed no crash point before it committed");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The peak RSS of one big transaction on `CHRONOS_TEST_ROWS` rows (see the file's comment): the process's high-water
/// mark is reset before it, so it is the transaction's.
#[test]
#[ignore]
fn big_transaction_peak_rss() {
    let rows_n: usize = std::env::var("CHRONOS_TEST_ROWS").ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let mode = std::env::var("CHRONOS_TEST_MODE").unwrap_or_else(|_| "copy".into());
    let dir = std::env::var("CHRONOS_TEST_DIR").map_or_else(|_| tmp("rss"), PathBuf::from);
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    let row = "i, i % 97, repeat('x', 84)";
    if mode != "ctas" {
        q(&db, &mut s, "create table t (id int primary key, g int, filler text)");
    }
    if mode == "update" {
        for at in (1..=rows_n).step_by(1_000_000) {
            let to = (at + 999_999).min(rows_n);
            q(&db, &mut s, &format!("insert into t select {row} from generate_series({at}, {to}) i"));
        }
    }
    let hwm = || {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        s.lines()
            .find(|l| l.starts_with("VmHWM"))
            .map_or(0, |l| l.split_whitespace().nth(1).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0) >> 10)
    };
    let _ = std::fs::write("/proc/self/clear_refs", "5"); // (resets VmHWM to the RSS now)
    let (start, before) = (std::time::Instant::now(), hwm());
    match mode.as_str() {
        // (`alone`: the same statements outside a transaction, to compare with)
        "insert" | "alone" => {
            // (generate_series makes up to 10 million rows at a time)
            if mode == "insert" {
                q(&db, &mut s, "begin");
            }
            for at in (1..=rows_n).step_by(10_000_000) {
                let to = (at + 9_999_999).min(rows_n);
                q(&db, &mut s, &format!("insert into t select {row} from generate_series({at}, {to}) i"));
            }
            if mode == "insert" {
                q(&db, &mut s, "commit");
            }
        }
        "copy" => {
            q(&db, &mut s, "begin");
            let c = s.copy_in(&db, &parse("copy t from stdin csv").unwrap().pop().unwrap()).unwrap();
            let (mut i, filler) = (1usize, "x".repeat(84));
            let next = move || {
                if i > rows_n {
                    return Ok(None);
                }
                let mut piece = String::with_capacity(1 << 17);
                while i <= rows_n && piece.len() < 1 << 16 {
                    piece += &format!("{i},{},{filler}\n", i % 97);
                    i += 1;
                }
                Ok(Some(piece.into_bytes()))
            };
            c.run(&mut s, &db, Box::new(next)).unwrap();
            q(&db, &mut s, "commit");
        }
        "ctas" => {
            q(&db, &mut s, &format!("create table t as select {row} from generate_series(1, {rows_n}) i"));
        }
        "update" => {
            q(&db, &mut s, "begin");
            q(&db, &mut s, "update t set g = g + 1");
            q(&db, &mut s, "commit");
        }
        m => panic!("CHRONOS_TEST_MODE {m}: insert, copy, ctas, update or alone"),
    }
    let took = start.elapsed();
    eprintln!(
        "{mode} of {rows_n} rows {}: peak RSS {} MB (RSS before it {before} MB), {:.1} s",
        if mode == "alone" { "outside a transaction" } else { "in a transaction" },
        hwm(),
        took.as_secs_f64()
    );
    assert_eq!(one(&db, &mut s, "select count(*) from t"), rows_n.to_string());
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
