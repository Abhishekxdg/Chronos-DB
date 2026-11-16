//! A scan checks a condition that compares an integer column with a whole number on the decoded integer, without
//! working the expression out, and reads a text column into the text its buffer already holds. Each must give what
//! the long way gives: the same rows for every operator, nulls and the widest numbers included, and each row its
//! own text (never the last row's) where a text is null, missing, or of another length.

use chronos::sql::{Value, run};
use chronos::{Db, MAIN};

fn rows(db: &Db, q: &str) -> Vec<Vec<Value>> {
    run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap().rows
}

fn open(name: &str) -> Db {
    let dir = std::env::temp_dir().join(format!("chronos-scan-fast-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Db::open(&dir).unwrap()
}

#[test]
fn integer_conditions_checked_on_the_decoded_number() {
    let db = open("ints");
    rows(&db, "create table t (id bigint primary key, s smallint, n integer, b bigint, f float8, x text)");
    // 20,000 rows (read in parts, on every core): nulls every 7th, negatives, and the ends of bigint
    rows(
        &db,
        "insert into t select i, i % 100 - 50, case when i % 7 = 0 then null else i % 1000 - 500 end, \
         case i % 5 when 0 then 9223372036854775807 when 1 then -9223372036854775808 else i - 10000 end, \
         i / 3.0, 'x' || i from generate_series(1, 20000) i",
    );
    for col in ["s", "n", "b"] {
        for op in ["=", "<>", "<", "<=", ">", ">="] {
            for k in ["0", "-50", "49", "500", "-501", "9223372036854775807", "-9223372036854775808"] {
                // the column compared as it is, and through an expression (worked out for each row)
                let quick = rows(&db, &format!("select count(*), sum(id) from t where {col} {op} {k}"));
                let long = rows(&db, &format!("select count(*), sum(id) from t where {col} + 0 {op} {k}"));
                assert_eq!(quick, long, "{col} {op} {k}");
                // the number first
                let flipped = rows(&db, &format!("select count(*), sum(id) from t where {k} {op} {col}"));
                let long = rows(&db, &format!("select count(*), sum(id) from t where {k} {op} {col} + 0"));
                assert_eq!(flipped, long, "{k} {op} {col}");
            }
        }
    }
    // several conditions, one of them not an integer's
    let both = rows(&db, "select count(*) from t where n > 100 and s < 10 and f > 50.5 and x like 'x1%'");
    let long = rows(&db, "select count(*) from t where n + 0 > 100 and s + 0 < 10 and f > 50.5 and x like 'x1%'");
    assert_eq!(both, long);
    assert_eq!(rows(&db, "select count(*) from t where n is null and n > 0"), vec![vec![Value::Int(0)]]);
    // a float column and a number with a fraction aren't checked this way
    assert_eq!(
        rows(&db, "select count(*) from t where f > 100"),
        rows(&db, "select count(*) from t where f + 0 > 100")
    );
    assert_eq!(rows(&db, "select count(*) from t where n > 0.5"), rows(&db, "select count(*) from t where n >= 1"));
}

#[test]
fn each_row_reads_its_own_text() {
    let db = open("texts");
    rows(&db, "create table t (id bigint primary key, name text, n integer)");
    // names null, empty, short and long, in turn; 12,000 rows
    let name =
        "case i % 4 when 0 then null when 1 then '' when 2 then 'n' || i else repeat('long', i % 9 + 1) || i end";
    rows(&db, &format!("insert into t select i, {name}, i from generate_series(1, 12000) i"));
    let expect = |i: i64| match i % 4 {
        0 => Value::Null,
        1 => Value::Text(String::new()),
        2 => Value::Text(format!("n{i}")),
        _ => Value::Text(format!("{}{i}", "long".repeat((i % 9 + 1) as usize))),
    };
    let check = |db: &Db| {
        // every row's own text, read into one buffer after another (a fold) and into rows
        let got = rows(db, "select id, name from t order by id");
        assert_eq!(got.len(), 12000);
        for r in &got {
            let Value::Int(i) = r[0] else { panic!("{r:?}") };
            assert_eq!(r[1], expect(i), "row {i}");
        }
        let folded =
            rows(db, "select n % 4, count(name), max(name), min(name), sum(length(name)) from t group by n % 4");
        let long = rows(
            db,
            "select n % 4, count(name || ''), max(name || ''), min(name || ''), sum(length(name || '')) \
             from t group by n % 4",
        );
        assert_eq!(folded, long);
        assert_eq!(rows(db, "select count(*) from t where name is null"), vec![vec![Value::Int(3000)]]);
    };
    check(&db);
    // rows written before a column was added have no field for it, and are read beside rows that do
    rows(&db, "alter table t add column note text");
    rows(&db, "update t set note = 'note ' || id where id % 3 = 0");
    check(&db);
    let notes = rows(&db, "select count(note), min(note), max(length(note)) from t");
    assert_eq!(notes, vec![vec![Value::Int(4000), Value::Text("note 10002".into()), Value::Int(10)]]);
    let per = rows(&db, "select id, note from t where id between 5998 and 6003 order by id");
    let want: Vec<Vec<Value>> = (5998..=6003)
        .map(|i| vec![Value::Int(i), if i % 3 == 0 { Value::Text(format!("note {i}")) } else { Value::Null }])
        .collect();
    assert_eq!(per, want);
}
