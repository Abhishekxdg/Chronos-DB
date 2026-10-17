//! Whole-table rewrites of a table too big to hold (ADD PRIMARY KEY, ALTER COLUMN ... TYPE, a generated column added,
//! CLUSTER of a table without a key) go on in parts whose writes spill to disk, and commit all at once through a
//! checkpoint (see `sql::rewrite`). This file's process runs with a 256 kB work budget, so a few thousand rows go in
//! parts; it checks that a rewrite in parts leaves what the same table made another way has, that duplicates and nulls
//! are found across parts (and nothing is written), that a kill part way leaves the table as it was or as it became,
//! and that its memory doesn't grow with the table.
//!
//! `big_add_primary_key_peak_rss` (ignored) is the measurement on a real size: run it alone, one size at a time, as
//! `CHRONOS_TEST_ROWS=5000000 cargo test --release --test big_rewrites -- --ignored --exact big_add_primary_key_peak_rss
//! --nocapture` (under `systemd-run --user --scope -p MemoryMax=30G` on the VM); it prints the process's peak RSS.

use chronos::sql::run;
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
    let d = std::env::temp_dir().join(format!("chronos-rewrite-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn sql(db: &Db, q: &str) -> Vec<Vec<String>> {
    let o = run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    o.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect()).collect()
}

fn one(db: &Db, q: &str) -> String {
    sql(db, q)[0][0].clone()
}

/// The error a statement fails with: code, message and detail.
fn fails(db: &Db, q: &str) -> (String, String, String) {
    match run(db, MAIN, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code.to_string(), e.message, e.info.and_then(|i| i.detail).unwrap_or_default()),
    }
}

fn spilled(db: &Db) -> u64 {
    db.counters().into_iter().find(|(n, _)| *n == "spilled_bytes").map_or(0, |(_, v)| v)
}

fn spill_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir.join("spill")).map_or(true, |mut d| d.next().is_none())
}

/// Every row under `prefix`, without it.
fn keys(db: &Db, prefix: &str) -> Vec<(String, String)> {
    db.prefix(MAIN, prefix).unwrap().into_iter().map(|(k, v)| (k[prefix.len()..].to_string(), v.to_string())).collect()
}

/// A table's index entries and unique rows (as `t`'s, whatever its name).
fn entries(db: &Db, t: &str) -> [Vec<(String, String)>; 2] {
    [keys(db, &format!("_sonos_idx/{t}/{t}_g/")), keys(db, &format!("_sonos_uniq/{t}/{t}_u_key/"))]
}

const ROW: &str = "i, i % 97, 'u' || i, repeat(md5(i::text), 6)";

/// Table `t`, without a key or with `key`, and its index on g, with rows `from..=to` (ids backwards, so the rows
/// written without a key are in no key order).
fn table(db: &Db, t: &str, key: &str, from: i64, to: i64) {
    sql(db, &format!("create table {t} (id int {key}, g int, u text unique, body text)"));
    sql(db, &format!("create index {t}_g on {t} (g)"));
    sql(db, &format!("insert into {t} select {ROW} from generate_series({to}, {from}, -1) i"));
}

const SAME: &[&str] = &[
    "select count(*), sum(g), max(u), min(body) from t",
    "select id, g, u from t where id between 4000 and 4010 order by id",
    "select id from t where g = 5 order by id limit 5",
    "select id from t where u = 'u4321'",
];

fn same(db: &Db, other: &str) {
    for q in SAME {
        assert_eq!(sql(db, q), sql(db, &q.replace(" t", &format!(" {other}"))), "{q}");
    }
}

#[test]
fn add_primary_key_in_parts_leaves_what_a_keyed_table_has() {
    let _one = setup();
    let dir = tmp("pkey");
    let db = Db::open(&dir).unwrap();
    table(&db, "t", "", 1, 6000);
    table(&db, "s", "primary key", 1, 6000);
    let before = spilled(&db);
    sql(&db, "alter table t add primary key (id)");
    assert!(spilled(&db) > before, "6,000 rows of 200 bytes don't fit in 256 kB: it went in parts");
    assert!(spill_empty(&dir), "its runs are gone");
    // rows under their keys, and every index row made again for them, as a table keyed from the start has them
    let ids = |t: &str| keys(&db, &format!("{t}/")).into_iter().map(|(k, _)| k).collect::<Vec<_>>();
    assert_eq!(ids("t"), ids("s"));
    assert_eq!(entries(&db, "t"), entries(&db, "s"));
    same(&db, "s");
    assert_eq!(one(&db, "select count(*) from pg_constraint where conname = 't_pkey' and contype = 'p'"), "1");
    assert_eq!(
        fails(&db, "insert into t values (17, 1, 'x', 'y')").0,
        "23505",
        "the key holds for the rows written from now on"
    );
    assert_eq!(fails(&db, "insert into t values (null, 1, 'x', 'y')").0, "23502");
    db.verify().unwrap();
    // (the same, as pg_dump writes it, with a several-column key)
    table(&db, "c", "", 1, 4000);
    sql(&db, "alter table only c add constraint c_key primary key (g, id)");
    assert_eq!(one(&db, "select count(*) from c where g = 3 and id = 3"), "1");
    assert_eq!(one(&db, "select count(*) from c"), "4000");
    db.verify().unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    same(&db, "s");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_duplicate_or_null_key_across_parts_is_refused_and_leaves_the_table_as_it_was() {
    let _one = setup();
    let dir = tmp("dup");
    let db = Db::open(&dir).unwrap();
    table(&db, "t", "", 1, 6000);
    // a duplicate far from the row it repeats: in another part
    sql(&db, "update t set id = 4321 where id = 17");
    let rows = keys(&db, "t/");
    let (code, message, detail) = fails(&db, "alter table t add primary key (id)");
    assert_eq!(
        (code.as_str(), message.as_str(), detail.as_str()),
        ("23505", "could not create unique index \"t_pkey\"", "Key (id)=(4321) is duplicated.")
    );
    assert_eq!(keys(&db, "t/"), rows, "nothing moved");
    assert!(spill_empty(&dir));
    assert_eq!(one(&db, "select count(*) from pg_constraint where conrelid = 't'::regclass and contype = 'p'"), "0");
    sql(&db, "update t set id = null where id = 4321 and u = 'u17'");
    let (code, message, _) = fails(&db, "alter table t add primary key (id)");
    assert_eq!((code.as_str(), message.as_str()), ("23502", "column \"id\" of relation \"t\" contains null values"));
    sql(&db, "delete from t where id is null");
    sql(&db, "alter table t add primary key (id)");
    assert_eq!(one(&db, "select count(*) from t"), "5999");
    db.verify().unwrap();
    // a new type for the key: its rows move to their keys as bigint writes them
    sql(&db, "create table k (id int primary key, body text)");
    sql(&db, "create index k_body on k (body)");
    sql(&db, "insert into k select i, repeat(md5(i::text), 6) from generate_series(1, 6000) i");
    sql(&db, "alter table k alter column id type bigint");
    assert_eq!(sql(&db, "select id, pg_typeof(id) from k where id = 4321"), [["4321", "bigint"]]);
    assert_eq!(one(&db, "select count(*) from k"), "6000");
    assert_eq!(one(&db, "select id from k where body = repeat(md5('17'), 6)"), "17");
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn types_generated_columns_and_cluster_in_parts() {
    let _one = setup();
    let dir = tmp("types");
    let db = Db::open(&dir).unwrap();
    table(&db, "t", "primary key", 1, 6000);
    table(&db, "s", "primary key", 1, 6000);
    let before = spilled(&db);
    // a new type by USING, on an indexed column: every row's value, and the index made again for them
    sql(&db, "alter table t alter column g type text using (g * 2)::text");
    sql(&db, "alter table s alter column g type text using (g * 2)::text");
    assert!(spilled(&db) > before, "in parts");
    assert_eq!(sql(&db, "select count(*) from t where g = '10'"), [["62"]]);
    assert_eq!(sql(&db, "select pg_typeof(g) from t limit 1"), [["text"]]);
    let n = |q: &str| one(&db, q);
    assert_eq!(n("select count(*) from t where g = '10'"), n("select count(*) from t where g || '' = '10'"));
    // a value the new type can't hold: nothing changes
    let (code, message, _) = fails(&db, "alter table t alter column u type int using u::int");
    assert_eq!(code, "22P02", "{message}");
    assert_eq!(sql(&db, "select pg_typeof(u) from t limit 1"), [["text"]]);
    let q = "select count(*), max(g), min(u), max(body) from t where g in ('10', '12', '190')";
    assert_eq!(sql(&db, q), sql(&db, &q.replace(" t", " s")));
    // a generated column: every row gets its value
    sql(&db, "alter table t add column twice int generated always as (id * 2) stored");
    assert_eq!(n("select count(*) from t where twice = id * 2"), "6000");
    assert_eq!(sql(&db, "select twice from t where id = 4321"), [["8642"]]);
    db.verify().unwrap();
    // CLUSTER of a table without a key: its rows in the index's order, under new keys
    sql(&db, "create table c (id int, g int, body text)");
    sql(&db, "create index c_g on c (g, id)");
    sql(&db, "insert into c select i, (i * 7919) % 6000, repeat(md5(i::text), 6) from generate_series(1, 6000) i");
    let before = spilled(&db);
    sql(&db, "cluster c using c_g");
    assert!(spilled(&db) > before, "in parts");
    let order: Vec<String> = sql(&db, "select g from c").into_iter().map(|r| r[0].clone()).collect();
    let mut sorted = order.clone();
    sorted.sort_by_key(|g| g.parse::<i64>().unwrap());
    assert_eq!(order, sorted, "the rows read in the index's order");
    assert_eq!(n("select count(*) from c"), "6000");
    assert_eq!(n("select count(*) from c where g = 7"), "1");
    assert_eq!(n("select count(distinct id) from c"), "6000");
    db.verify().unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(sql(&db, "select twice from t where id = 4321"), [["8642"]]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn memory_doesnt_grow_with_the_table() {
    let _one = setup();
    let dir = tmp("memory");
    let db = Db::open(&dir).unwrap();
    let measure = |f: &mut dyn FnMut()| {
        let base = NOW.load(Relaxed);
        PEAK.store(base, Relaxed);
        f();
        PEAK.load(Relaxed) - base
    };
    // (the same statement on 20,000 and 80,000 rows, from a page cache filled the same way)
    let grew: Vec<usize> = [("a", 20_000), ("b", 80_000)]
        .into_iter()
        .map(|(t, n)| {
            table(&db, t, "", 1, n);
            sql(&db, "select count(*) from generate_series(1, 10)");
            measure(&mut || {
                sql(&db, &format!("alter table {t} add primary key (id)"));
            })
        })
        .collect();
    eprintln!("add primary key: heap growth 20,000 rows {} kB, 80,000 rows {} kB", grew[0] >> 10, grew[1] >> 10);
    // (held in memory, as before: 53 MB for 20,000 rows, 208 MB for 80,000)
    assert!(grew[1] < grew[0] + (4 << 20), "{grew:?}: four times the rows, about the same memory");
    assert!(grew[1] < 48 << 20, "{grew:?}");
    assert_eq!(one(&db, "select count(*) from b where id between 100 and 199"), "100");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

const CHILD: &str = "CHRONOS_REWRITE_CHILD";

/// The child process of `a_kill_mid_rewrite_leaves_the_table_whole` (a no-op otherwise).
#[test]
fn child() {
    let Ok(dir) = std::env::var(CHILD) else { return };
    unsafe { std::env::set_var("CHRONOS_WORK_MEM", "256kB") };
    let db = Db::open(&dir).unwrap();
    sql(&db, "alter table t add primary key (id)");
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

#[test]
fn a_kill_mid_rewrite_leaves_the_table_whole() {
    let _one = setup();
    let (base, dir) = (tmp("kill-base"), tmp("kill"));
    {
        let db = Db::open(&base).unwrap();
        table(&db, "t", "", 1, 60_000);
    }
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
        let finished = child.try_wait().unwrap().is_some_and(|s| s.success());
        let _ = child.kill(); // SIGKILL
        child.wait().unwrap();
        let spilling = !spill_empty(&dir);
        let db = Db::open(&dir).unwrap_or_else(|e| panic!("killed after {ms} ms: {e}"));
        assert!(spill_empty(&dir), "opening empties the spill folder");
        db.verify().unwrap_or_else(|e| panic!("killed after {ms} ms: {e}"));
        let keyed = one(&db, "select count(*) from pg_constraint where conname = 't_pkey'") == "1";
        // the rows all under their old keys (40 hex digits) or all under their ids, and every index row there
        let rows = keys(&db, "t/");
        let numbered = rows.iter().filter(|(k, _)| k.len() < 40).count();
        let [ix, uq] = entries(&db, "t");
        eprintln!("killed after {ms} ms: keyed {keyed}{}", if spilling { ", while spilling" } else { "" });
        assert_eq!((rows.len(), ix.len(), uq.len()), (60_000, 60_000, 60_000), "killed after {ms} ms");
        assert_eq!(numbered, if keyed { 60_000 } else { 0 }, "killed after {ms} ms: never a mix");
        assert_eq!(one(&db, "select count(*) from t where g = 5"), "619");
        assert_eq!(one(&db, "select id from t where u = 'u4321'"), "4321");
        mid += (spilling && !keyed) as usize;
        drop(db);
        if finished || keyed {
            break;
        }
        ms += 100;
        assert!(ms < 600_000, "the statement never finished");
    }
    assert!(mid >= 1, "no kill came while the statement was spilling");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The peak RSS of ADD PRIMARY KEY on `CHRONOS_TEST_ROWS` rows (see the file's comment), or of the statements
/// `CHRONOS_TEST_SQL` (`;` between them) after `CHRONOS_TEST_PREP`'s: the process's high-water mark is reset after the
/// rows are loaded, so it is the statement's.
#[test]
#[ignore]
fn big_add_primary_key_peak_rss() {
    let rows: i64 = std::env::var("CHRONOS_TEST_ROWS").ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let dir = std::env::var("CHRONOS_TEST_DIR").map_or_else(|_| tmp("rss"), PathBuf::from);
    let what = std::env::var("CHRONOS_TEST_SQL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "alter table t add primary key (id)".into());
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    sql(&db, "create table t (id int, filler text)");
    for q in std::env::var("CHRONOS_TEST_PREP").unwrap_or_default().split(';').filter(|q| !q.trim().is_empty()) {
        sql(&db, q);
    }
    let step = 1_000_000;
    for at in (1..=rows).step_by(step as usize) {
        let to = (at + step - 1).min(rows);
        sql(&db, &format!("insert into t select i, repeat('x', 84) from generate_series({to}, {at}, -1) i"));
    }
    let hwm = || {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        s.lines()
            .find(|l| l.starts_with("VmHWM"))
            .map_or(0, |l| l.split_whitespace().nth(1).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0) >> 10)
    };
    let loaded = hwm();
    let _ = std::fs::write("/proc/self/clear_refs", "5"); // (resets VmHWM to the RSS now)
    let (start, before) = (std::time::Instant::now(), hwm());
    for q in what.split(';').filter(|q| !q.trim().is_empty()) {
        sql(&db, q);
    }
    let took = start.elapsed();
    eprintln!(
        "{what} on {rows} rows: peak RSS {} MB (RSS before it {before} MB; loading peaked at {loaded} MB), {:.1} s",
        hwm(),
        took.as_secs_f64()
    );
    assert_eq!(one(&db, "select count(*) from t"), rows.to_string());
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
