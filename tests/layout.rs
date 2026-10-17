//! Leaves of wide rows end by size too (see `prolly::boundary`). A database written before that,
//! whose leaves ended by key alone, reads, diffs, merges and verifies as it did, and takes new
//! writes. The fixture tests/fixtures/key-only-leaves was written by v0.1.2 (f67070f3) from
//! bench/fixtures-gen/key-only-leaves.sql; the values below are what v0.1.2 read from it
//! (bench/fixtures-gen/key-only-leaves-check.sql).

use chronos::Db;
use chronos::sql::{Session, Value};

fn rows(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    out.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn run(db: &Db, s: &mut Session, q: &str) -> String {
    s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap().tag
}

fn copy(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for f in std::fs::read_dir(from).unwrap() {
        let f = f.unwrap();
        match f.file_type().unwrap().is_dir() {
            true => copy(&f.path(), &to.join(f.file_name())),
            false => drop(std::fs::copy(f.path(), to.join(f.file_name())).unwrap()),
        }
    }
}

const WIDE: &str = "select count(*), sum(length(body)), md5(string_agg(body, ',' order by id)) from wide";
const NARROW: &str = "select count(*), sum(n) from narrow";

#[test]
fn a_database_from_before_sizes_counted_reads_diffs_and_merges_the_same() {
    let from = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/key-only-leaves");
    let dir = std::env::temp_dir().join(format!("chronos-layout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy(&from, &dir);
    // what v0.1.2 read: main, then world w1, then main after merging w1
    let (main_wide, main_narrow) = {
        let db = Db::open(&dir).unwrap();
        let s = &mut Session::new(chronos::MAIN);
        assert_eq!(rows(&db, s, WIDE), ["400|512032|7336105dc9ce4c33e63357aba627435f"]);
        assert_eq!(rows(&db, s, NARROW), ["2000|14007000"]);
        run(&db, s, "switch world w1");
        assert_eq!(rows(&db, s, WIDE), ["400|513720|569dfbf386b1616aaebf42898a1c2654"]);
        run(&db, s, "switch world main");
        // w1's own changes: 8 updated, 1 deleted and 1 new wide row, 20 narrow rows
        assert_eq!(rows(&db, s, "diff world w1").len(), 30);
        assert_eq!(run(&db, s, "merge world w1"), "MERGE 30");
        assert_eq!(rows(&db, s, WIDE), ["400|513752|ff14f70f9b670ab3f4bd67676c680c48"]);
        assert_eq!(rows(&db, s, NARROW), ["2000|13713000"]);
        run(&db, s, "verify database");

        // writes now, on the old trees: leaves they touch end by size from here on
        run(&db, s, "update wide set body = body || 'x' where id % 3 = 0");
        run(&db, s, "insert into wide select g, repeat(md5(g::text), 60) from generate_series(2000, 2100) g");
        run(&db, s, "delete from wide where id between 200 and 220");
        run(&db, s, "update narrow set n = n + 1 where id % 7 = 0");
        let after = (rows(&db, s, WIDE), rows(&db, s, NARROW));
        db.checkpoint().unwrap();
        db.close().unwrap();
        after
    };
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(chronos::MAIN);
    assert_eq!(rows(&db, s, WIDE), main_wide, "the same rows after a checkpoint and reopening");
    assert_eq!(rows(&db, s, NARROW), main_narrow);
    run(&db, s, "verify database");
    // a world forked from the mixed tree: its diff and merge see just its rows
    run(&db, s, "create world w2");
    run(&db, s, "switch world w2");
    assert_eq!(run(&db, s, "update wide set body = upper(body) where id < 50"), "UPDATE 48"); // 7 is gone
    run(&db, s, "switch world main");
    assert_eq!(rows(&db, s, "diff world w2").len(), 48);
    assert_eq!(run(&db, s, "merge world w2"), "MERGE 48");
    assert_eq!(rows(&db, s, "select count(*) from wide where body = upper(body) and id < 50"), ["48"]);
    db.checkpoint().unwrap();
    run(&db, s, "verify database");
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
