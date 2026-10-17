//! Statements too big to hold in memory: past CHRONOS_WORK_MEM an INSERT, UPDATE, DELETE or COPY
//! goes on in parts whose writes spill to disk, and commits all at once through a checkpoint.
//! This file's process runs with a 256 kB work budget, so a few thousand rows spill; it checks
//! that such a statement leaves exactly what the same rows written in small statements leave,
//! that its checks (keys, UNIQUE, foreign keys) see across its parts, that it's all or nothing
//! (errors, and a kill -9 part way), that worlds, merges and time travel see it whole, that it
//! works encrypted, and that its memory doesn't grow with it.

use chronos::sql::{Session, parse, run};
use chronos::{Db, MAIN, Open, Resolve};
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
    let d = std::env::temp_dir().join(format!("chronos-bulk-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn on(db: &Db, b: &str, q: &str) -> Vec<Vec<String>> {
    let o = run(db, b, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    o.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect()).collect()
}

fn sql(db: &Db, q: &str) -> Vec<Vec<String>> {
    on(db, MAIN, q)
}

fn one(db: &Db, q: &str) -> String {
    sql(db, q)[0][0].clone()
}

fn fails(db: &Db, q: &str) -> String {
    match run(db, MAIN, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => format!("{}: {}", e.code, e.described()), // (the row that holds a key is in the HINT)
    }
}

fn spilled(db: &Db) -> u64 {
    db.counters().into_iter().find(|(n, _)| *n == "spilled_bytes").map_or(0, |(_, v)| v)
}

/// Every row under `prefix` in `b`, without the prefix.
fn keys(db: &Db, b: &str, prefix: &str) -> Vec<(String, String)> {
    db.prefix(b, prefix).unwrap().into_iter().map(|(k, v)| (k[prefix.len()..].to_string(), v.to_string())).collect()
}

/// A table's rows, index entries and unique rows.
fn stored(db: &Db, b: &str, t: &str) -> [Vec<(String, String)>; 3] {
    [
        keys(db, b, &format!("{t}/")),
        keys(db, b, &format!("_sonos_idx/{t}/{t}_g/")),
        keys(db, b, &format!("_sonos_uniq/{t}/{t}_u_key/")),
    ]
}

fn table(db: &Db, b: &str, t: &str) {
    on(db, b, &format!("create table {t} (id int primary key, g int, u text unique, body text)"));
    on(db, b, &format!("create index {t}_g on {t} (g)"));
}

const ROW: &str = "i, i % 97, 'u' || i, repeat(md5(i::text), 6)";

fn spill_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir.join("spill")).map_or(true, |mut d| d.next().is_none())
}

#[test]
fn a_big_insert_spills_and_leaves_what_small_statements_leave() {
    let _one = setup();
    let dir = tmp("insert");
    let db = Db::open(&dir).unwrap();
    table(&db, MAIN, "t");
    table(&db, MAIN, "s");
    let before = spilled(&db);
    assert_eq!(
        sql(&db, &format!("insert into t select {ROW} from generate_series(1, 6000) i")),
        Vec::<Vec<String>>::new()
    );
    assert!(spilled(&db) > before, "6,000 rows of 200 bytes don't fit in 256 kB: it spilled");
    assert!(spill_empty(&dir), "its runs are gone");
    // the same rows, 100 a statement: each held in memory
    for at in (0..6000).step_by(100) {
        sql(&db, &format!("insert into s select {ROW} from generate_series({}, {}) i", at + 1, at + 100));
    }
    let (a, b) = (stored(&db, MAIN, "t"), stored(&db, MAIN, "s"));
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.len(), 6000);
        assert!(x == y, "the same rows and entries");
    }
    for q in [
        "select count(*), sum(g), max(u) from t",
        "select count(*) from t where g = 5",
        "select id from t where u = 'u4321'",
    ] {
        assert_eq!(sql(&db, q), sql(&db, &q.replace(" t", " s")), "{q}");
    }
    assert!(fails(&db, "insert into t values (7000, 1, 'u5', 'x')").contains("t_u_key"));
    // a multi-row VALUES and an INSERT ... SELECT from a table go in parts too
    let values: Vec<String> = (10_001..=12_000).map(|i| format!("({i}, 1, 'v{i}', '{}')", "x".repeat(150))).collect();
    sql(&db, &format!("insert into t values {}", values.join(",")));
    sql(&db, "create table t2 (id int primary key, g int, u text unique, body text)");
    let before = spilled(&db);
    sql(&db, "insert into t2 select * from t where id <= 6000");
    assert!(spilled(&db) > before);
    assert_eq!(one(&db, "select count(*) from t2"), "6000");
    assert_eq!(sql(&db, "select sum(g), min(u) from t2"), sql(&db, "select sum(g), min(u) from t where id <= 6000"));
    // generate_series read a part at a time: the rows the SELECT alone returns, step and filter too
    let series = "from generate_series(1, 6000) i";
    assert_eq!(
        sql(&db, "select count(*), sum(g), max(u), min(body) from t where id <= 6000"),
        sql(&db, &format!("select count(*), sum(i % 97), max('u' || i), min(repeat(md5(i::text), 6)) {series}"))
    );
    sql(&db, "create table t3 (id int primary key, g int)");
    let before = spilled(&db);
    sql(&db, "insert into t3 select i, i % 7 from generate_series(40000, 1, -3) i where i % 2 = 0");
    assert!(spilled(&db) > before);
    // nextval() in a series read in parts: a value for each row (their order and gaps are as on
    // main, where the SELECT is also evaluated on every core and the sequence reserves in blocks)
    sql(&db, "create sequence sq");
    sql(&db, "create table t5 (id int primary key, g int)");
    sql(&db, "insert into t5 select nextval('sq'), i from generate_series(1, 20000) i");
    assert_eq!(
        sql(&db, "select count(*), count(distinct id), count(distinct g) from t5"),
        [["20000", "20000", "20000"]]
    );
    // a SELECT that can't be read in parts (ORDER BY) is held, but its writes still spill
    sql(&db, "create table t4 (id int primary key, g int, u text unique, body text)");
    let before = spilled(&db);
    sql(&db, "insert into t4 select * from t where id <= 6000 order by id desc");
    assert!(spilled(&db) > before);
    assert_eq!(
        sql(&db, "select count(*), sum(g), max(u) from t4"),
        sql(&db, "select count(*), sum(g), max(u) from t2")
    );
    assert_eq!(
        sql(&db, "select count(*), sum(g), min(id), max(id) from t3"),
        sql(&db, "select count(*), sum(i % 7), min(i), max(i) from generate_series(40000, 1, -3) i where i % 2 = 0")
    );
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(stored(&db, MAIN, "t")[1].len(), 8000);
    assert_eq!(one(&db, "select count(*) from t"), "8000");
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn updates_and_deletes_in_parts_act_as_one_statement() {
    let _one = setup();
    let dir = tmp("update");
    let db = Db::open(&dir).unwrap();
    table(&db, MAIN, "t");
    sql(&db, &format!("insert into t select {ROW} from generate_series(1, 6000) i"));
    // every row takes the next row's unique value: fine as one statement, though each value is
    // still held when the row before takes it, and the rows span many parts
    let before = spilled(&db);
    assert_eq!(run(&db, MAIN, "update t set u = 'u' || (id + 1), g = g + 1", &[]).unwrap()[0].tag, "UPDATE 6000");
    assert!(spilled(&db) > before);
    assert_eq!(sql(&db, "select count(distinct u), min(u), max(g) from t"), [["6000", "u10", "97"]]);
    assert!(fails(&db, "insert into t values (0, 0, 'u2', 'x')").contains("HINT: Row t/1 already has this key."));
    assert_eq!(sql(&db, "select count(*) from t where g = 6"), sql(&db, "select count(*) from t where g + 0 = 6"));
    // moving every row to a key another row leaves
    sql(&db, "update t set id = id + 3000");
    assert_eq!(sql(&db, "select count(*), min(id), max(id) from t"), [["6000", "3001", "9000"]]);
    assert_eq!(sql(&db, "select u from t where id = 3001"), [["u2"]]);
    assert_eq!(stored(&db, MAIN, "t").map(|x| x.len()), [6000; 3]);
    // two rows taking one value, far apart: nothing changes
    let was = stored(&db, MAIN, "t");
    let e = fails(&db, "update t set u = case when id in (3001, 8999) then 'same' else u || 'x' end");
    assert!(e.starts_with("23505") && e.contains("t_u_key"), "{e}");
    let e = fails(&db, "update t set id = case when id = 8999 then 10001 else id + 7000 end"); // 3001's new key
    assert!(e.starts_with("23505") && e.contains("t_pkey"), "{e}");
    assert!(stored(&db, MAIN, "t") == was, "all or nothing");
    assert!(spill_empty(&dir));
    // deleting half, in parts
    assert_eq!(run(&db, MAIN, "delete from t where id % 2 = 0", &[]).unwrap()[0].tag, "DELETE 3000");
    assert_eq!(stored(&db, MAIN, "t").map(|x| x.len()), [3000; 3]);
    sql(&db, "insert into t values (2, 0, 'u5', 'x')"); // u5 was row 3004's, now gone
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(one(&db, "select count(*) from t"), "3001");
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clashes_across_parts_leave_nothing() {
    let _one = setup();
    let dir = tmp("clash");
    let db = Db::open(&dir).unwrap();
    table(&db, MAIN, "t");
    // the same key twice, 5,000 rows apart
    let e =
        fails(&db, "insert into t select i % 5000 + 1, 0, 'u' || i, repeat('x', 200) from generate_series(1, 6000) i");
    assert!(e.starts_with("23505") && e.contains("t_pkey"), "{e}");
    // the same unique value twice
    let e =
        fails(&db, "insert into t select i, 0, 'u' || (i % 5000), repeat('x', 200) from generate_series(1, 6000) i");
    assert!(e.starts_with("23505") && e.contains("t_u_key"), "{e}");
    assert_eq!(one(&db, "select count(*) from t"), "0");
    // a value a row already has, at row 4,000
    sql(&db, "insert into t values (1, 0, 'u4000', 'x')");
    let e = fails(&db, &format!("insert into t select {ROW} from generate_series(2, 6000) i"));
    assert!(e.starts_with("23505") && e.contains("HINT: Row t/1 already has this key."), "{e}");
    assert_eq!(stored(&db, MAIN, "t").map(|x| x.len()), [1; 3]);
    assert!(spill_empty(&dir));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn foreign_keys_hold_across_parts() {
    let _one = setup();
    let dir = tmp("fk");
    let db = Db::open(&dir).unwrap();
    sql(&db, "create table p (id int primary key, body text)");
    sql(&db, "insert into p select i, repeat('p', 100) from generate_series(1, 6000) i");
    sql(&db, "create table c (id int primary key, p int references p(id), body text)");
    sql(&db, "insert into c select i, i % 10 + 1, repeat('c', 200) from generate_series(1, 6000) i");
    assert_eq!(one(&db, "select count(*) from c"), "6000");
    // one row, far in, names a parent that isn't there
    let e = fails(
        &db,
        "insert into c select i, case when i = 10000 then 7000 else 1 end, repeat('c', 200) from generate_series(6001, 12000) i",
    );
    assert!(e.starts_with("23503"), "{e}");
    assert_eq!(one(&db, "select count(*) from c"), "6000");
    // deleting parents that children still name: refused, whole
    let e = fails(&db, "delete from p where id > 5");
    assert!(e.starts_with("23503"), "{e}");
    assert_eq!(one(&db, "select count(*) from p"), "6000");
    assert_eq!(run(&db, MAIN, "delete from p where id > 10", &[]).unwrap()[0].tag, "DELETE 5990");
    // ON DELETE CASCADE from a big delete: held in memory, and right
    sql(&db, "create table d (id int primary key, p int references p(id) on delete cascade)");
    sql(&db, "insert into d select i, i % 10 + 1 from generate_series(1, 3000) i");
    sql(&db, "delete from c");
    sql(&db, "delete from p where id > 5");
    assert_eq!(one(&db, "select count(*) from d"), "1500");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn worlds_merges_history_and_time_travel_see_it_whole() {
    let _one = setup();
    let dir = tmp("worlds");
    let db = Db::open(&dir).unwrap();
    table(&db, MAIN, "t");
    sql(&db, "insert into t select i, 0, 'm' || i, 'x' from generate_series(1, 10) i");
    db.checkpoint().unwrap();
    db.fork(MAIN, "w").unwrap();
    let t0 = now_ms();
    std::thread::sleep(std::time::Duration::from_millis(5));
    on(&db, "w", &format!("insert into t select {ROW} from generate_series(11, 6010) i"));
    assert_eq!(sql(&db, "select count(*) from t"), [["10"]], "main doesn't see the world's statement");
    assert_eq!(on(&db, "w", "select count(*) from t"), [["6010"]]);
    assert_eq!(db.diff("w").unwrap().len(), 6000);
    let events = db.history("w", 10).unwrap();
    assert!(events.iter().any(|e| e.what == "write" && e.rows >= 6000), "{events:?}");
    db.merge("w", Resolve::Fail).unwrap();
    assert_eq!(one(&db, "select count(*) from t"), "6010");
    assert_eq!(stored(&db, MAIN, "t").map(|x| x.len()), [6010; 3]);
    // as they were: the world before its statement, main before its own big one
    assert_eq!(db.view_at("w", t0).unwrap().prefix("t/").unwrap().count(), 10);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let t1 = now_ms();
    std::thread::sleep(std::time::Duration::from_millis(5));
    sql(&db, "update t set g = 1, body = 'y'");
    assert_eq!(sql(&db, "select count(*) from t where g = 1"), [["6010"]]);
    let was = |db: &Db| {
        db.view_at(MAIN, t1).unwrap().prefix("t/").unwrap().filter(|r| r.as_ref().unwrap().1.ends_with('y')).count()
    };
    assert_eq!(was(&db), 0, "before the update, no body was 'y'");
    // after reopening: the statement's note is replayed without changing anything
    let version = db.version(MAIN).unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.version(MAIN).unwrap(), version);
    assert_eq!(sql(&db, "select count(*) from t where g = 1"), [["6010"]]);
    assert_eq!(was(&db), 0);
    // inside a transaction: the statement spills into it, and COMMIT brings it
    sql(&db, "create table x (id int primary key, body text)");
    let mut s = Session::new(MAIN);
    s.run(&db, "begin", &[]).unwrap();
    s.run(&db, "insert into x select i, repeat('x', 200) from generate_series(1, 5000) i", &[]).unwrap();
    assert_eq!(one(&db, "select count(*) from x"), "0");
    s.run(&db, "commit", &[]).unwrap();
    assert_eq!(one(&db, "select count(*) from x"), "5000");
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

#[test]
fn copy_goes_in_as_one_statement() {
    let _one = setup();
    let dir = tmp("copy");
    let db = Db::open(&dir).unwrap();
    table(&db, MAIN, "t");
    let copy = |data: &str| {
        let mut s = Session::new(MAIN);
        let st = parse("copy t from stdin csv").unwrap().pop().unwrap();
        let c = s.copy_in(&db, &st)?;
        let mut pieces: Vec<Vec<u8>> = data.as_bytes().chunks(10_000).map(<[u8]>::to_vec).collect();
        pieces.reverse();
        c.run(&mut s, &db, Box::new(move || Ok(pieces.pop())))
    };
    let rows: String = (1..=6000).map(|i| format!("{i},{},u{i},{}\n", i % 97, "b".repeat(200))).collect();
    let before = spilled(&db);
    assert_eq!(copy(&rows).unwrap().tag, "COPY 6000");
    assert!(spilled(&db) > before);
    assert_eq!(stored(&db, MAIN, "t").map(|x| x.len()), [6000; 3]);
    // a clash in the last row: none of them
    let more: String = (6001..=12000).map(|i| format!("{i},0,u{},x\n", if i == 12000 { 5 } else { i })).collect();
    let e = copy(&more).err().unwrap();
    assert_eq!(e.code, "23505", "{}", e.message);
    assert_eq!(one(&db, "select count(*) from t"), "6000");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Parts in key order into a table with no other index go straight into the new tree: no
/// runs are written, and the rows are what small statements leave. That's a load whose keys
/// sort as they come (integer keys in number order, text as text), and an UPDATE of values,
/// which reads in key order. A key repeated after such parts still clashes, and then nothing is
/// written.
#[test]
fn parts_in_key_order_write_no_runs() {
    let _one = setup();
    let dir = tmp("ordered");
    let db = Db::open(&dir).unwrap();
    for t in ["v", "w"] {
        on(&db, MAIN, &format!("create table {t} (id text primary key, body text)"));
    }
    on(&db, MAIN, "create table n (id int primary key, body text)");
    let before = spilled(&db);
    sql(&db, "insert into n select i, repeat(md5(i::text), 6) from generate_series(1, 6000) i");
    assert_eq!(spilled(&db), before, "integer keys in order: no runs");
    sql(&db, "insert into v select lpad(i::text, 6, '0'), repeat(md5(i::text), 6) from generate_series(1, 6000) i");
    assert_eq!(spilled(&db), before, "keys in order: no runs");
    sql(&db, "update n set body = upper(body)");
    assert_eq!(spilled(&db), before, "an UPDATE of values: no runs");
    for from in (1..=6000).step_by(500) {
        let to = from + 499;
        sql(
            &db,
            &format!(
                "insert into w select lpad(i::text, 6, '0'), repeat(md5(i::text), 6) from generate_series({from}, {to}) i"
            ),
        );
    }
    assert_eq!(keys(&db, MAIN, "v/"), keys(&db, MAIN, "w/"));
    assert_eq!(one(&db, "select count(*) from n where body = upper(repeat(md5(id::text), 6))"), "6000");
    // parts in order, then a key from the first of them again: its part takes a run, and clashes
    let e = fails(
        &db,
        "insert into v select lpad(case when i = 12000 then 6001 else i end::text, 6, '0'), 'x' from generate_series(6001, 12000) i",
    );
    assert!(e.starts_with("23505"), "{e}");
    assert_eq!(one(&db, "select count(*) from v"), "6000");
    assert!(spill_empty(&dir));
    db.verify().unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(sql(&db, "select count(*), max(id) from v"), [["6000", "006000"]]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn encrypted_databases_spill_sealed() {
    let _one = setup();
    let dir = tmp("sealed");
    let key = Some([7u8; 32]);
    let db = Db::open_with(&dir, &Open { key, ..Default::default() }).unwrap();
    table(&db, MAIN, "t");
    sql(&db, &format!("insert into t select {ROW} from generate_series(1, 6000) i"));
    sql(&db, "update t set body = 'secret' || id");
    assert!(spill_empty(&dir));
    drop(db);
    let db = Db::open_with(&dir, &Open { key, ..Default::default() }).unwrap();
    assert_eq!(sql(&db, "select count(*), min(body) from t"), [["6000", "secret1"]]);
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn memory_doesnt_grow_with_the_statement() {
    let _one = setup();
    let dir = tmp("memory");
    let db = Db::open(&dir).unwrap();
    table(&db, MAIN, "t");
    // first fills the page cache (CHRONOS_CACHE_MB), which both measured statements then start with
    sql(&db, &format!("insert into t select {ROW} from generate_series(1000001, 1080000) i"));
    let measure = |f: &mut dyn FnMut()| {
        let base = NOW.load(Relaxed);
        PEAK.store(base, Relaxed);
        f();
        PEAK.load(Relaxed) - base
    };
    let flat = |what: &str, grew: [usize; 2]| {
        eprintln!("{what}: heap growth 20,000 rows {} kB, 80,000 rows {} kB", grew[0] >> 10, grew[1] >> 10);
        assert!(grew[1] < grew[0] + (4 << 20), "{what} {grew:?}: four times the rows, about the same memory");
        assert!(grew[1] < 48 << 20, "{what} {grew:?}"); // a part of rows as they come (16,384), and a batch of changes
    };
    let sizes = [(1, 20_000), (100_001, 80_000)];
    flat(
        "insert",
        sizes.map(|(from, n)| {
            measure(&mut || {
                sql(&db, &format!("insert into t select {ROW} from generate_series({from}, {}) i", from + n - 1));
            })
        }),
    );
    // `id + 0`: a key range up to 100,000 keys wide holds the rows it reads (see the operations guide)
    flat(
        "update",
        sizes.map(|(from, n)| {
            measure(&mut || {
                sql(
                    &db,
                    &format!(
                        "update t set g = g + 1, body = upper(body) where id + 0 between {from} and {}",
                        from + n - 1
                    ),
                );
            })
        }),
    );
    table(&db, MAIN, "c");
    flat(
        "copy",
        sizes.map(|(from, n)| {
            let data: String = (from..from + n).map(|i| format!("{i},{},u{i},{}\n", i % 97, "b".repeat(200))).collect();
            let mut data = Some(data.into_bytes()); // handed over, not copied, inside the measure
            measure(&mut || {
                let mut s = Session::new(MAIN);
                let c = s.copy_in(&db, &parse("copy c from stdin csv").unwrap().pop().unwrap()).unwrap();
                let (data, mut at) = (data.take().unwrap(), 0);
                let next = move || {
                    let piece =
                        data.get(at..(at + 65_536).min(data.len())).filter(|p| !p.is_empty()).map(<[u8]>::to_vec);
                    at += 65_536;
                    Ok(piece)
                };
                c.run(&mut s, &db, Box::new(next)).unwrap();
            })
        }),
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

const CHILD: &str = "CHRONOS_BULK_CHILD";

/// The child process of `a_kill_mid_statement_leaves_nothing` (a no-op otherwise).
#[test]
fn child() {
    let Ok(dir) = std::env::var(CHILD) else { return };
    unsafe { std::env::set_var("CHRONOS_WORK_MEM", "256kB") };
    let db = Db::open(&dir).unwrap();
    sql(&db, &format!("insert into t select {ROW} from generate_series(1001, 60000) i"));
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
fn a_kill_mid_statement_leaves_nothing() {
    let _one = setup();
    let (base, dir) = (tmp("kill-base"), tmp("kill"));
    {
        let db = Db::open(&base).unwrap();
        table(&db, MAIN, "t");
        sql(&db, &format!("insert into t select {ROW} from generate_series(1, 1000) i"));
    }
    let exe = std::env::current_exe().unwrap();
    let (mut mid, mut ms) = (0, 200);
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
        let n = one(&db, "select count(*) from t");
        let lens = stored(&db, MAIN, "t").map(|x| x.len());
        eprintln!("killed after {ms} ms: {n} rows{}", if spilling { ", while spilling" } else { "" });
        match n.as_str() {
            "1000" => {
                assert_eq!(lens, [1000; 3]);
                mid += spilling as usize;
            }
            "60000" => assert_eq!(lens, [60000; 3]),
            _ => panic!("killed after {ms} ms: {n} rows, neither before nor after"),
        }
        drop(db);
        if finished || n == "60000" {
            break;
        }
        ms += 200;
        assert!(ms < 600_000, "the statement never finished");
    }
    assert!(mid >= 1, "no kill came while the statement was spilling");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}
