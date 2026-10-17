//! Primary keys of several columns: rows kept in their key's order, looked up, referenced by
//! foreign keys, forked, merged, reopened, and shown and typed as `(1,abc)` in diffs, merges, the
//! shell and the HTTP API. And one-column keys as main's binary wrote them still read the same.

mod common;

use chronos::Db;
use chronos::sql::{Session, Value};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn rows(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    out.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> chronos::sql::SqlError {
    s.run(db, q, &[]).err().unwrap_or_else(|| panic!("{q} should fail"))
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chronos-keys-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn several_column_keys_through_forks_merges_and_reopening() {
    let dir = temp("fork");
    {
        let db = Db::open(&dir).unwrap();
        let s = &mut Session::new(chronos::MAIN);
        for q in [
            "create table ol (ord int, line int, note text, primary key (ord, line))",
            "create table pay (id int primary key, ord int, line int, foreign key (ord, line) references ol on delete cascade)",
            "create table ev (who text, day date, n int, primary key (who, day))",
            "insert into ol values (10, 1, 'j'), (1, 2, 'b'), (2, 1, 'c'), (1, 1, 'a'), (-5, 3, 'neg')",
            "insert into ev values ('a b', '2024-01-02', 1), ('a,b', '2024-01-01', 2), ('', '2024-01-03', 3), ('a b', '2024-01-01', 4)",
            "insert into pay values (1, 1, 1), (2, 2, 1)",
        ] {
            s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
        }
        // the table's own order is its key's, numbers as numbers (no ORDER BY here)
        assert_eq!(rows(&db, s, "select ord, line from ol"), ["-5|3", "1|1", "1|2", "2|1", "10|1"]);
        assert_eq!(rows(&db, s, "select who, n from ev"), ["|3", "a b|4", "a b|1", "a,b|2"]);
        assert_eq!(rows(&db, s, "select note from ol where ord = 1 and line = 2"), ["b"]);
        assert_eq!(rows(&db, s, "select note from ol where ord = 1"), ["a", "b"]);

        // a world changes rows by their keys; DIFF shows the keys as rows
        s.run(&db, "create world w; use world w", &[]).unwrap();
        s.run(&db, "update ol set note = 'w' where ord = 1 and line = 2; insert into ol values (3, 1, 'new'); delete from ol where ord = 2 and line = 1", &[]).unwrap();
        let out = s.run(&db, "diff", &[]).unwrap().pop().unwrap();
        let ids: Vec<String> =
            out.rows.iter().filter(|r| r[0] == Value::Text("ol".into())).map(|r| r[1].to_text()).collect();
        assert_eq!(ids, ["(1,2)", "(2,1)", "(3,1)"], "{out:?}");
        let sql = s.run(&db, "diff as sql", &[]).unwrap().pop().unwrap();
        let sql: Vec<String> = sql.rows.iter().map(|r| r[0].to_text()).collect();
        assert!(sql.contains(&"update ol set note = 'w' where ord = 1 and line = 2;".to_string()), "{sql:?}");
        assert!(sql.contains(&"delete from ol where ord = 2 and line = 1;".to_string()), "{sql:?}");
        // the cascade went with the delete
        assert_eq!(rows(&db, s, "select id from pay"), ["1"]);
        s.run(&db, "use world main", &[]).unwrap();
        s.run(&db, "update ol set note = 'm' where ord = 10 and line = 1", &[]).unwrap();
        s.run(&db, "merge world w", &[]).unwrap();
        assert_eq!(rows(&db, s, "select ord, line, note from ol"), ["-5|3|neg", "1|1|a", "1|2|w", "3|1|new", "10|1|m"]);
        assert_eq!(rows(&db, s, "select id from pay"), ["1"]);

        // a conflict names its row as people read it, and RESOLVE takes that form
        s.run(
            &db,
            "create world w2; use world w2; update ol set note = 'x' where ord = 1 and line = 1; use world main",
            &[],
        )
        .unwrap();
        s.run(&db, "update ol set note = 'y' where ord = 1 and line = 1", &[]).unwrap();
        let e = fails(&db, s, "merge world w2");
        assert!(e.code == "40001" && e.message.contains("ol/(1,1)"), "{e:?}");
        let preview = s.run(&db, "merge world w2 dry run", &[]).unwrap().pop().unwrap();
        assert!(preview.rows.iter().any(|r| r[1] == Value::Text("(1,1)".into())), "{preview:?}");
        s.run(&db, "merge world w2 resolve ('ol/(1,1)' = ours)", &[]).unwrap();
        assert_eq!(rows(&db, s, "select note from ol where ord = 1 and line = 1"), ["x"]);
        // a text key's world, merged into main
        s.run(
            &db,
            "create world w3; use world w3; insert into ev values ('a b', '2024-01-03', 5); use world main",
            &[],
        )
        .unwrap();
        s.run(&db, "merge world w3", &[]).unwrap();
        db.close().unwrap();
    }
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(chronos::MAIN);
    assert_eq!(rows(&db, s, "select ord, line, note from ol"), ["-5|3|neg", "1|1|x", "1|2|w", "3|1|new", "10|1|m"]);
    assert_eq!(rows(&db, s, "select n from ev where who = 'a b'"), ["4", "1", "5"]);
    assert_eq!(fails(&db, s, "insert into ol values (1, 2, 'again')").code, "23505");
    assert_eq!(fails(&db, s, "insert into pay values (9, 7, 7)").code, "23503");
    s.run(&db, "delete from ol where ord = 1 and line = 1", &[]).unwrap();
    assert_eq!(rows(&db, s, "select count(*) from pay"), ["0"], "the cascade still works after reopening");
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A database folder written by main's binary before keys of several columns (tables with one
/// column keys, integer and text, a foreign key, a unique constraint, an index, a JSON table),
/// copied from tests/fixtures: it reads and writes as it did.
#[test]
fn one_column_keys_from_before_read_the_same() {
    let from = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/single-keys");
    let dir = temp("old");
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
    copy(&from, &dir);
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(chronos::MAIN);
    assert_eq!(rows(&db, s, "select id, name from users order by id"), ["1|Ada", "2|Alan", "10|Grace"]);
    assert_eq!(rows(&db, s, "select sku, qty from items"), ["a|3", "b b|1", "c/1|7"]);
    assert_eq!(rows(&db, s, "select title from posts where id = 2"), ["second"]);
    assert_eq!(rows(&db, s, "select title from posts where author = 1 order by id"), ["first", "second"]);
    assert_eq!(rows(&db, s, "select qty from items where sku = 'b b'"), ["1"]);
    assert_eq!(rows(&db, s, "select id from users where email = 'grace@x'"), ["10"]);
    assert_eq!(rows(&db, s, "select note from notes where id = 'n1'"), ["hi"]);
    assert_eq!(fails(&db, s, "insert into users values (1, 'dup', 'd@x')").code, "23505");
    assert_eq!(fails(&db, s, "insert into users values (11, 'dup', 'ada@x')").code, "23505");
    assert_eq!(fails(&db, s, "delete from users where id = 1").code, "23503");
    assert_eq!(fails(&db, s, "insert into posts values (9, 99, 'orphan')").code, "23503");
    s.run(&db, "insert into users values (3, 'Edsger', 'e@x'); update items set qty = qty + 1 where sku = 'c/1'", &[])
        .unwrap();
    assert_eq!(rows(&db, s, "select qty from items where sku = 'c/1'"), ["8"]);
    // the stored keys are the ones main wrote: `users/1`, `items/c/1`
    assert!(db.get(chronos::MAIN, "users/10").unwrap().is_some());
    assert!(db.get(chronos::MAIN, "items/c/1").unwrap().is_some());
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Tables made now keep an integer key in number order: rows scan as numbers, a range of keys
/// (one-sided too) is one read of the tree, and people still see and type `t/7` everywhere:
/// diffs, ONLY KEYS, foreign keys, and the key-value API (which takes only `7`, not `007`).
#[test]
fn integer_keys_in_number_order() {
    let db = Db::new();
    let s = &mut Session::new(chronos::MAIN);
    s.run(&db, "create table t (id bigint primary key, v text)", &[]).unwrap();
    s.run(
        &db,
        "insert into t values (1000, 'k'), (-5, 'm'), (99, 'n'), (0, 'z'), (7, 's'), (10, 't'), (123456789012, 'b')",
        &[],
    )
    .unwrap();
    assert_eq!(rows(&db, s, "select id from t"), ["-5", "0", "7", "10", "99", "1000", "123456789012"]);
    assert_eq!(rows(&db, s, "select id from t where id between 7 and 99"), ["7", "10", "99"]);
    assert_eq!(rows(&db, s, "select id from t where id > 99"), ["1000", "123456789012"]);
    assert_eq!(rows(&db, s, "select id from t where id < 0"), ["-5"]);
    assert_eq!(rows(&db, s, "select v from t where id = 7"), ["s"]);
    assert_eq!(rows(&db, s, "select v from t where id in (10, 0, 5)"), ["z", "t"]);
    let plan = rows(&db, s, "explain select * from t where id > 99").join("\n");
    assert!(plan.contains("Index Scan using t_pkey"), "{plan}");
    // stored in number order, read and written as people type keys
    let stored = chronos::sql::row_key_of(&db, chronos::MAIN, "t/7");
    assert_ne!(stored, "t/7");
    assert_eq!(chronos::sql::show_key(&stored), "t/7");
    assert!(db.view(chronos::MAIN).unwrap().get(&stored).unwrap().is_some());
    assert_eq!(&*db.get(chronos::MAIN, "t/7").unwrap().unwrap(), r#"{"id":7,"v":"s"}"#);
    db.put(chronos::MAIN, "t/8", r#"{"v": "eight"}"#).unwrap();
    assert_eq!(rows(&db, s, "select v from t where id = 8"), ["eight"]);
    assert!(db.put(chronos::MAIN, "t/008", r#"{"v": "x"}"#).is_err(), "a key is written as its number");
    db.del(chronos::MAIN, "t/8").unwrap();
    assert!(rows(&db, s, "select v from t where id = 8").is_empty());
    // foreign keys reference the number
    s.run(&db, "create table c (id int primary key, t_id bigint references t(id))", &[]).unwrap();
    s.run(&db, "insert into c values (1, 7)", &[]).unwrap();
    assert_eq!(fails(&db, s, "insert into c values (2, 8)").code, "23503");
    assert_eq!(fails(&db, s, "delete from t where id = 7").code, "23503");
    // diffs and ONLY KEYS
    db.fork(chronos::MAIN, "w").unwrap();
    let w = &mut Session::new("w");
    w.run(&db, "insert into t values (11, 'x'), (12, 'y')", &[]).unwrap();
    let keys: Vec<String> = db.diff("w").unwrap().iter().map(|c| chronos::sql::show_key(&c.key).into_owned()).collect();
    assert!(keys.contains(&"t/11".to_string()) && keys.contains(&"t/12".to_string()), "{keys:?}");
    s.run(&db, "merge world w only keys ('t/11')", &[]).unwrap();
    assert_eq!(rows(&db, s, "select id from t where id > 10 and id < 100"), ["11", "99"]);
}

/// A date's, a number's or a boolean's key is kept in the form an index sorts it by. The key-value API takes
/// it as people read it (`d/2026-01-01`) and reaches the row SQL has: a put used to write a second row with
/// the same primary key. NaN and the infinities are keys too, in numeric's order.
#[test]
fn keys_of_dates_numbers_and_booleans_typed_as_read() {
    let db = Db::new();
    let s = &mut Session::new(chronos::MAIN);
    s.run(&db, "create table d (day date primary key, v text)", &[]).unwrap();
    s.run(&db, "insert into d values ('2026-01-01', 'new year')", &[]).unwrap();
    let stored = chronos::sql::row_key_of(&db, chronos::MAIN, "d/2026-01-01");
    assert_ne!(stored, "d/2026-01-01");
    assert_eq!(chronos::sql::show_key(&stored), "d/2026-01-01");
    assert!(db.get(chronos::MAIN, "d/2026-01-01").unwrap().is_some());
    db.put(chronos::MAIN, "d/2026-01-01", r#"{"v": "again"}"#).unwrap();
    assert_eq!(rows(&db, s, "select day, v from d"), ["2026-01-01|again"], "one row, not two of one key");
    db.put(chronos::MAIN, "d/2026-02-03", r#"{"v": "feb"}"#).unwrap();
    assert_eq!(rows(&db, s, "select v from d where day = '2026-02-03'"), ["feb"]);
    db.del(chronos::MAIN, "d/2026-01-01").unwrap();
    assert_eq!(rows(&db, s, "select day from d"), ["2026-02-03"]);

    s.run(&db, "create table n (k numeric primary key, v text)", &[]).unwrap();
    s.run(
        &db,
        "insert into n values (1.50, 'a'), ('NaN', 'nan'), ('Infinity', 'inf'), ('-Infinity', 'ninf'), (-3, 'm')",
        &[],
    )
    .unwrap();
    assert_eq!(rows(&db, s, "select v from n order by k"), ["ninf", "m", "a", "inf", "nan"]);
    assert_eq!(rows(&db, s, "select v from n where k = 'NaN'"), ["nan"]);
    assert_eq!(rows(&db, s, "select v from n where k = '-Infinity'"), ["ninf"]);
    assert_eq!(fails(&db, s, "insert into n values ('NaN', 'dup')").code, "23505");
    assert_eq!(fails(&db, s, "insert into n values ('Infinity', 'dup')").code, "23505");
    assert!(db.get(chronos::MAIN, "n/1.5").unwrap().is_some());
    db.put(chronos::MAIN, "n/1.5", r#"{"v": "b"}"#).unwrap();
    assert_eq!(rows(&db, s, "select v from n where k = 1.5"), ["b"]);
    assert_eq!(rows(&db, s, "select count(*) from n"), ["5"]);

    s.run(&db, "create table b (ok boolean primary key, v text)", &[]).unwrap();
    s.run(&db, "insert into b values (true, 'y')", &[]).unwrap();
    assert!(db.get(chronos::MAIN, "b/true").unwrap().is_some());
    db.put(chronos::MAIN, "b/false", r#"{"v": "n"}"#).unwrap();
    db.put(chronos::MAIN, "b/true", r#"{"v": "yes"}"#).unwrap();
    assert_eq!(rows(&db, s, "select ok, v from b order by ok"), ["false|n", "true|yes"]);
}

/// A row written as JSON takes a missing column's default. One only SQL can work out (it takes a sequence's
/// value) says so, rather than failing with a message about sessions.
#[test]
fn a_json_row_and_a_default_only_sql_works_out() {
    let db = Db::new();
    let s = &mut Session::new(chronos::MAIN);
    s.run(&db, "create sequence js", &[]).unwrap();
    s.run(&db, "create table j (id int primary key, n int default nextval('js') + 1, v text)", &[]).unwrap();
    let e = format!("{:?}", db.put(chronos::MAIN, "j/1", r#"{"v": "x"}"#).unwrap_err());
    assert!(e.contains("only SQL can work out") && e.contains("nextval"), "{e}");
    db.put(chronos::MAIN, "j/1", r#"{"n": 5, "v": "x"}"#).unwrap();
    assert_eq!(rows(&db, s, "select id, n, v from j"), ["1|5|x"]);
    s.run(&db, "insert into j (id, v) values (2, 'y')", &[]).unwrap();
    assert_eq!(rows(&db, s, "select n from j where id = 2"), ["2"]);
}

fn chronos(dir: &std::path::Path, args: &[&str], stdin: &str) -> (bool, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .arg(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let o = c.wait_with_output().unwrap();
    (o.status.success(), String::from_utf8(o.stdout).unwrap(), String::from_utf8(o.stderr).unwrap())
}

/// The shell and the HTTP API take a key of several columns as `(1,abc)`, and show it that way.
#[test]
fn shell_and_http_read_keys_as_rows() {
    let dir = temp("shell");
    let (ok, out, err) = chronos(
        &dir,
        &[],
        "create table ol (ord int, line int, note text, primary key (ord, line));
insert into ol values (1, 2, 'b');
get ol (1,2)
put ol (1,3) {\"note\": \"json\"}
fork f
put ol (1,2) {\"note\": \"forked\"}
diff
use main
select ord, line, note from ol;
",
    );
    assert!(ok, "{err}\n{out}");
    assert!(out.contains("\"note\": \"b\""), "{out}");
    assert!(out.contains("~ ol/(1,2)  note: \"b\" -> \"forked\""), "{out}");
    assert!(out.contains("1 | 3 | json") || out.contains("json"), "{out}");

    // over HTTP: POST /v1/get with the key as a row
    let srv = common::serve(Command::new(env!("CARGO_BIN_EXE_chronos")).args(["serve", dir.to_str().unwrap()]), false);
    let (mut server, port) = (srv.child, srv.http);
    let post = |op: &str, body: &str| -> String {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            c,
            "POST /v1/{op} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        out
    };
    let got = post("get", r#"{"table": "ol", "id": "(1,3)"}"#);
    assert!(got.contains("\"note\":\"json\""), "{got}");
    let diff = post("diff", r#"{"branch": "f"}"#);
    assert!(diff.contains("\"key\":\"ol/(1,2)\""), "{diff}");
    let _ = server.kill();
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
