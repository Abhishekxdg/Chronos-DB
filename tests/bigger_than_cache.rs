//! A database ten times its page cache: queries answer right and the cache stays under its cap.
//! This file's process runs with a 4 MB page cache and a 1 MB CHRONOS_WORK_MEM, so the search
//! index budget (an eighth of it) is far below the tables here.

use chronos::Db;
use chronos::sql::{Output, Value, run};

fn on(db: &Db, branch: &str, q: &str) -> Output {
    run(db, branch, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap()
}

fn sql(db: &Db, q: &str) -> Output {
    on(db, chronos::MAIN, q)
}

fn one(db: &Db, q: &str) -> String {
    sql(db, q).rows[0].iter().map(Value::to_text).collect::<Vec<_>>().join("|")
}

fn counter(db: &Db, name: &str) -> u64 {
    db.counters().into_iter().find(|(n, _)| *n == name).map_or_else(|| panic!("no {name}"), |(_, v)| v)
}

fn setup(name: &str) -> std::path::PathBuf {
    static ENV: std::sync::Once = std::sync::Once::new();
    // set before anything reads them (each is read once per process or per open)
    ENV.call_once(|| unsafe {
        std::env::set_var("CHRONOS_CACHE_MB", "4");
        std::env::set_var("CHRONOS_WORK_MEM", "1MB");
    });
    let dir = std::env::temp_dir().join(format!("chronos-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

const ROWS: i64 = 150_000;

#[test]
fn queries_over_ten_times_the_cache() {
    let dir = setup("bigger");
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table t (id bigint primary key, g int, tag text, body text)");
        sql(&db, "create table d (k text primary key, n bigint)");
        // bodies of distinct md5s barely compress: about 300 bytes a row, over 40 MB in all
        for start in (0..ROWS).step_by(10_000) {
            let body = (0..8).map(|j| format!("md5((i * 8 + {j})::text)")).collect::<Vec<_>>().join(" || ");
            sql(
                &db,
                &format!(
                    "insert into t select i, i % 100, 'tag-' || (i % 1000), {body} from generate_series({start}, {}) i",
                    start + 9_999
                ),
            );
            sql(
                &db,
                &format!("insert into d select md5(i::text), i from generate_series({start}, {}) i", start + 9_999),
            );
        }
        db.checkpoint().unwrap();
        let size = counter(&db, "pages_bytes");
        assert!(size > 40 << 20, "the data is ten times the cache: {size}");
    }
    let db = Db::open(&dir).unwrap(); // cold
    let cap = 4 << 20;
    let checks: Vec<(String, String)> = vec![
        ("select count(*), sum(g), min(length(body)), max(length(body)) from t".into(), format!("{ROWS}|{}|256|256", (0..ROWS).map(|i| i % 100).sum::<i64>())),
        ("select g, count(*) from t group by g order by g desc limit 1".into(), format!("99|{}", ROWS / 100)),
        ("select count(*) from t where tag = 'tag-7'".into(), (ROWS / 1000).to_string()),
        ("select count(*), sum(id) from t where id between 5000 and 5999".into(), format!("1000|{}", (5000..6000).sum::<i64>())),
        ("select body = md5('8000') || md5('8001') || md5('8002') || md5('8003') || md5('8004') || md5('8005') || md5('8006') || md5('8007') from t where id = 1000".into(), "true".into()),
        ("select count(*) from t a join t b on b.id = a.g where a.id < 500".into(), "500".into()),
    ];
    for (q, want) in &checks {
        assert_eq!(one(&db, q), *want, "{q}");
        assert!(counter(&db, "cache_bytes") <= cap, "{q}: cache over its cap");
    }
    // a text-key range reads its slice, and agrees with a scan
    let range = "select count(*), sum(n) from d where k >= 'ab' and k < 'ac'";
    assert_eq!(one(&db, range), one(&db, &range.replace("k ", "k || '' ")));
    assert!(counter(&db, "cache_misses") > 1000 && counter(&db, "cache_hits") > 0);
    assert!(counter(&db, "cache_bytes") <= cap);
    // a fork with changes: its own rows and the shared ones
    db.fork(chronos::MAIN, "f").unwrap();
    on(&db, "f", "update t set tag = 'changed' where id between 1000 and 1099 -- 1007 was tag-7");
    assert_eq!(on(&db, "f", "select count(*) from t where tag = 'changed'").rows[0][0], Value::Int(100));
    assert_eq!(on(&db, "f", "select count(*) from t where tag = 'tag-7'").rows[0][0], Value::Int(ROWS / 1000 - 1));
    assert!(counter(&db, "cache_bytes") <= cap);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unique_checks_on_a_table_over_the_search_budget() {
    let dir = setup("uniq");
    let db = Db::open(&dir).unwrap();
    sql(&db, "create table u (id bigint primary key, email text unique, n int)");
    // 20,000 rows: far more than the 128 kB a query may build a search index for
    sql(&db, "insert into u select i, 'u' || i || '@x.io', 0 from generate_series(1, 20000) i");
    db.checkpoint().unwrap();
    sql(&db, "insert into u values (20001, 'late@x.io', 0)"); // since the checkpoint
    assert_eq!(
        sql(&db, "insert into u values (30000, 'u77@x.io', 1) on conflict (email) do nothing").tag,
        "INSERT 0 0"
    );
    assert_eq!(
        sql(&db, "insert into u values (30000, 'late@x.io', 1) on conflict (email) do nothing").tag,
        "INSERT 0 0"
    );
    sql(&db, "insert into u values (30000, 'u78@x.io', 1) on conflict (email) do update set n = u.n + 5");
    assert_eq!(one(&db, "select id, n from u where email = 'u78@x.io'"), "78|5");
    sql(&db, "insert into u values (30001, 'new@x.io', 1) on conflict on constraint u_email_key do update set n = 9");
    assert_eq!(one(&db, "select id, n from u where email = 'new@x.io'"), "30001|1");
    let e = run(&db, chronos::MAIN, "insert into u values (30002, 'u9@x.io', 1)", &[]).expect_err("a duplicate");
    assert_eq!(e.code, "23505");
    // an email that moved: its old value is free, its new one taken
    sql(&db, "update u set email = 'moved@x.io' where id = 10");
    sql(&db, "insert into u values (30003, 'u10@x.io', 1) on conflict (email) do nothing");
    assert_eq!(one(&db, "select id from u where email = 'u10@x.io'"), "30003");
    assert_eq!(
        sql(&db, "insert into u values (30004, 'moved@x.io', 1) on conflict (email) do nothing").tag,
        "INSERT 0 0"
    );
    // in a fork, its own rows count too
    db.fork(chronos::MAIN, "f").unwrap();
    on(&db, "f", "insert into u values (40000, 'fork@x.io', 0)");
    on(&db, "f", "insert into u values (40001, 'fork@x.io', 0) on conflict (email) do update set n = 7");
    assert_eq!(
        on(&db, "f", "select id, n from u where email = 'fork@x.io'").rows[0],
        [Value::Int(40000), Value::Int(7)]
    );
    assert_eq!(sql(&db, "select count(*) from u where email = 'fork@x.io'").rows[0][0], Value::Int(0));
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
