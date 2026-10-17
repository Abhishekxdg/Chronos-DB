//! COPY over the Postgres protocol, through real clients: psql (`\copy`, and COPY ... FROM STDIN
//! with data inline in a script, as pg_dump writes it) and the postgres crate (the extended
//! protocol). Where a Postgres runs at host=/tmp, the same psql scripts run there too and must
//! print the same. Skipped without psql.

mod common;

use chronos::sql::{Session, Value};
use postgres::{Client, NoTls};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(name: &str) -> (Server, u16) {
    let dir = std::env::temp_dir().join(format!("chronos-copy-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = common::serve(Command::new(env!("CARGO_BIN_EXE_chronos")).args(["serve", dir.to_str().unwrap()]), true);
    (Server(s.child, dir), s.pg)
}

fn have_psql() -> bool {
    let ok = Command::new("psql").arg("--version").output().is_ok();
    if !ok {
        eprintln!("skipped: psql not installed");
    }
    ok
}

/// Where psql connects: Chronos on `port`, or the reference Postgres (host=/tmp) in schema `schema`.
enum To<'a> {
    Chronos(u16),
    Postgres(&'a str),
}

/// Runs psql with these arguments and stdin; its stdout, then stderr, as text.
fn psql(to: &To, args: &[&str], stdin: &[u8]) -> (bool, Vec<u8>, String) {
    let mut cmd = Command::new("psql");
    match to {
        To::Chronos(port) => cmd.args(["-h", "127.0.0.1", "-p", &port.to_string(), "-d", "main"]),
        To::Postgres(schema) => cmd
            .env("PGOPTIONS", format!("-c search_path={schema} -c timezone=UTC"))
            .args(["-h", "/tmp", "-d", "postgres"]),
    };
    let mut child = cmd
        .args(["-X", "-q", "-v", "VERBOSITY=sqlstate", "-v", "SHOW_CONTEXT=never"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    (out.status.success(), out.stdout, String::from_utf8_lossy(&out.stderr).into_owned())
}

fn sql(to: &To, q: &str) -> String {
    let (ok, out, err) = psql(to, &["-A", "-t", "-v", "ON_ERROR_STOP=1", "-c", q], b"");
    assert!(ok, "{q}: {err}");
    String::from_utf8(out).unwrap()
}

/// A reference Postgres, in a fresh schema of this name (and this process's id, so test runs
/// sharing one Postgres, like CI jobs at once, don't collide); None if there's none.
fn reference(schema: &str) -> Option<To<'static>> {
    let schema: &'static str = Box::leak(format!("{schema}_{}", std::process::id()).into_boxed_str());
    let to = To::Postgres(schema);
    let (ok, _, _) =
        psql(&to, &["-c", &format!("drop schema if exists {schema} cascade; create schema {schema}")], b"");
    if !ok {
        eprintln!("no Postgres at host=/tmp: compared with Chronos alone");
        return None;
    }
    Some(to)
}

/// CSV as Postgres writes it: quoted when it holds a comma, quote, newline or is empty.
fn csv_field(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => (if *b { "t" } else { "f" }).into(),
        v => {
            let s = v.to_text();
            if s.is_empty() || s.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s
            }
        }
    }
}

/// 100,000 rows of every common type, tricky text included, in key order (as text: Chronos keeps
/// integer keys in text order, so `COPY t TO` gives them in this order).
fn rows_csv(n: i64) -> Vec<u8> {
    let names = ["plain", "with, comma", "a \"quote\"", "two\nlines", "", "tab\there", "back\\slash", "ünïcödé", "\\."];
    let ids: Vec<i64> = (1..=n).collect(); // in key order: integer keys as numbers
    let mut out = String::new();
    for i in ids {
        let name = if i % 11 == 0 { Value::Null } else { Value::Text(names[i as usize % names.len()].into()) };
        let score = if i % 13 == 0 { Value::Null } else { Value::Float((i % 1000) as f64 * 0.25 - 17.5) };
        let row = [
            Value::Int(i),
            name,
            score,
            Value::Bool(i % 3 == 0),
            Value::Text(format!("20{:02}-{:02}-{:02}", i % 30, i % 12 + 1, i % 28 + 1)),
            Value::Text(format!("2024-03-{:02} {:02}:{:02}:{:02}+00", i % 28 + 1, i % 24, i % 60, (i * 7) % 60)),
            Value::Text(format!("{{\"k\": {}, \"tags\": [\"a\", \"b,c\"]}}", i % 97)),
            Value::Text(format!("{}.{:02}", i % 100_000, i % 100)),
        ];
        out += &row.iter().map(csv_field).collect::<Vec<_>>().join(",");
        out.push('\n');
    }
    out.into_bytes()
}

const TABLE: &str = "create table t (id int primary key, name text, score double precision, ok boolean, born date, \
                     at timestamptz, doc jsonb, price numeric(12, 2))";

#[test]
fn psql_round_trip_is_byte_identical() {
    if !have_psql() {
        return;
    }
    let (_server, port) = start("roundtrip");
    let data = rows_csv(100_000);
    let mut targets = vec![To::Chronos(port)];
    targets.extend(reference("copy_roundtrip"));
    for to in &targets {
        sql(to, TABLE);
        let (ok, _, err) = psql(to, &["-v", "ON_ERROR_STOP=1", "-c", "copy t from stdin csv"], &data);
        assert!(ok, "{err}");
        assert_eq!(sql(to, "select count(*) from t"), "100000\n");
        // Postgres keeps rows in the order they came; Chronos in key order, which the data is in
        let q = match to {
            To::Chronos(_) => "copy t to stdout csv",
            To::Postgres(_) => "copy (select * from t order by id) to stdout csv",
        };
        let (ok, out, err) = psql(to, &["-c", q], b"");
        assert!(ok, "{err}");
        assert!(
            out == data,
            "COPY TO gave back different bytes (first difference at byte {:?})",
            out.iter().zip(&data).position(|(a, b)| a != b)
        );
        // and through psql's \copy, both ways, by files on the client
        let dir = std::env::temp_dir().join(format!("chronos-copy-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("in.csv"), dir.join("out.csv"));
        std::fs::write(&a, &data).unwrap();
        sql(
            to,
            "create table t2 (id int primary key, name text, score double precision, ok boolean, born date, at timestamptz, doc jsonb, price numeric(12, 2))",
        );
        sql(to, &format!("\\copy t2 from '{}' csv", a.display()));
        let copy_out = match to {
            To::Chronos(_) => format!("\\copy t2 to '{}' csv", b.display()),
            To::Postgres(_) => {
                format!("\\copy (select * from t2 order by id) to '{}' csv", b.display())
            }
        };
        sql(to, &copy_out);
        assert!(std::fs::read(&b).unwrap() == data, "\\copy round trip differs");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_failed_copy_leaves_nothing() {
    if !have_psql() {
        return;
    }
    let (_server, port) = start("failure");
    let to = To::Chronos(port);
    sql(&to, "create table t (id int primary key, v int)");
    let rows = |bad: &str| {
        let mut s = String::new();
        for i in 1..=50_000 {
            if i == 45_000 {
                s += bad;
            } else {
                s += &format!("{i},{i}\n");
            }
        }
        s.into_bytes()
    };
    // a bad value, and a duplicate key, far past the first batch of rows
    for (bad, code) in [("45000,oops\n", "22P02"), ("7,7\n", "23505"), ("45000,1,2\n", "22P04")] {
        let (ok, _, err) = psql(&to, &["-c", "copy t from stdin csv"], &rows(bad));
        assert!(!ok && err.contains(code), "{bad}: {err}");
        assert_eq!(sql(&to, "select count(*) from t"), "0\n", "{bad}");
    }
    // (where, in its CONTEXT, as Postgres says it)
    let (_, _, err) = psql(
        &to,
        &["-v", "SHOW_CONTEXT=always", "-v", "VERBOSITY=default", "-c", "copy t from stdin csv"],
        &rows("45000,oops\n"),
    );
    assert!(err.contains("CONTEXT:  COPY t, line 45000, column v: \"oops\""), "{err}");
    // inside a transaction: the transaction fails with it
    let script = "begin;\ninsert into t values (1, 1);\ncopy t from stdin csv;\n2,2\nx,3\n\\.\ncommit;\nselect count(*) from t;\n";
    let (_, out, _) = psql(&to, &["-A", "-t", "-f", "-"], script.as_bytes());
    assert_eq!(String::from_utf8(out).unwrap(), "0\n");

    // the driver: CopyFail (a writer dropped unfinished), and a connection lost mid-COPY
    let mut c = Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main"), NoTls).unwrap();
    let mut w = c.copy_in("copy t from stdin csv").unwrap();
    w.write_all(&rows("45000,45000\n")).unwrap();
    drop(w);
    assert_eq!(c.query_one("select count(*) from t", &[]).unwrap().get::<_, i64>(0), 0);
    // a client that goes away mid-COPY, past the first batch: the protocol by hand
    let mut raw = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let msg = |tag: u8, body: &[u8]| [&[tag][..], &(body.len() as i32 + 4).to_be_bytes(), body].concat();
    let hello = b"user\0tester\0database\0main\0\0";
    raw.write_all(&[&(hello.len() as i32 + 8).to_be_bytes()[..], &196608i32.to_be_bytes(), hello].concat()).unwrap();
    let until = |raw: &mut std::net::TcpStream, want: u8| loop {
        let mut head = [0u8; 5];
        raw.read_exact(&mut head).unwrap();
        let mut body = vec![0; i32::from_be_bytes(head[1..].try_into().unwrap()) as usize - 4];
        raw.read_exact(&mut body).unwrap();
        if head[0] == want {
            break;
        }
    };
    until(&mut raw, b'Z');
    raw.write_all(&msg(b'Q', b"copy t from stdin csv\0")).unwrap();
    until(&mut raw, b'G');
    for piece in rows("45000,45000\n")[..500_000].chunks(8192) {
        raw.write_all(&msg(b'd', piece)).unwrap();
    }
    drop(raw);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(sql(&to, "select count(*) from t"), "0\n");
    // and a good one goes in whole
    let (ok, _, err) = psql(&to, &["-c", "copy t from stdin csv"], &rows("45000,45000\n"));
    assert!(ok, "{err}");
    assert_eq!(sql(&to, "select count(*), sum(v) from t"), format!("50000|{}\n", 50_000i64 * 50_001 / 2));
}

#[test]
fn driver_copy_in_and_out() {
    let (_server, port) = start("driver");
    let mut c = Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main"), NoTls).unwrap();
    c.batch_execute("create table t (id int primary key, name text, v float8 default 0.5)").unwrap();
    let mut w = c.copy_in("COPY t (id, name) FROM STDIN").unwrap();
    w.write_all(b"1\tAda\n2\t\\N\n3\ta\\tb\n").unwrap();
    assert_eq!(w.finish().unwrap(), 3);
    let mut out = String::new();
    c.copy_out("COPY t TO STDOUT WITH (FORMAT csv, HEADER)").unwrap().read_to_string(&mut out).unwrap();
    assert_eq!(out, "id,name,v\n1,Ada,0.5\n2,,0.5\n3,a\tb,0.5\n");
    let e = c.copy_in("COPY t FROM '/etc/passwd'").err().unwrap();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"));
    // prepared statements in SQL share the protocol's namespace
    c.batch_execute("prepare by_id (int) as select name from t where id = $1").unwrap();
    let rows = c.simple_query("execute by_id(1)").unwrap();
    assert!(rows.iter().any(|m| matches!(m, postgres::SimpleQueryMessage::Row(r) if r.get(0) == Some("Ada"))));
    c.batch_execute("deallocate by_id").unwrap();
    assert_eq!(c.batch_execute("execute by_id(1)").unwrap_err().code().map(|c| c.code()), Some("26000"));
}

/// A READ ONLY transaction (or default_transaction_read_only) copies into a temporary table, and refuses a COPY into
/// any other before the data comes (25006, by COPY FROM's name), failing the transaction, as Postgres's DoCopy does.
#[test]
fn copy_from_in_a_read_only_transaction() {
    let (_server, port) = start("read-only");
    let mut c = Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main"), NoTls).unwrap();
    c.batch_execute("create table ro (a int)").unwrap();
    c.batch_execute("create temp table ro_tmp (a int)").unwrap();
    c.batch_execute("begin read only").unwrap();
    let mut w = c.copy_in("copy ro_tmp from stdin").unwrap();
    w.write_all(b"1\n2\n").unwrap();
    assert_eq!(w.finish().unwrap(), 2);
    assert_eq!(c.query_one("select count(*) from ro_tmp", &[]).unwrap().get::<_, i64>(0), 2);
    let e = c.copy_in("copy ro from stdin").err().unwrap();
    assert_eq!(e.code().map(|c| c.code()), Some("25006"));
    assert_eq!(e.as_db_error().unwrap().message(), "cannot execute COPY FROM in a read-only transaction");
    assert_eq!(c.batch_execute("select 1").unwrap_err().code().map(|c| c.code()), Some("25P02"));
    c.batch_execute("rollback").unwrap();
    c.batch_execute("set default_transaction_read_only = on").unwrap();
    let mut w = c.copy_in("copy ro_tmp from stdin").unwrap();
    w.write_all(b"3\n").unwrap();
    assert_eq!(w.finish().unwrap(), 1);
    let e = c.copy_in("copy ro from stdin").err().unwrap();
    assert_eq!(e.code().map(|c| c.code()), Some("25006"));
    c.batch_execute("reset default_transaction_read_only").unwrap();
    assert_eq!(c.query_one("select count(*) from ro", &[]).unwrap().get::<_, i64>(0), 0);
    assert_eq!(c.query_one("select count(*) from ro_tmp", &[]).unwrap().get::<_, i64>(0), 1);
}

/// A COPY FROM STDIN that fails before any data (an unknown column, found at Execute) leaves the
/// connection ready for the next one: the driver ends the failed one with CopyFail + Sync.
#[test]
fn driver_copy_in_after_a_failed_start() {
    let (_server, port) = start("failed-start");
    let mut c = Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main"), NoTls).unwrap();
    c.batch_execute("create table x (id int primary key)").unwrap();
    let e = c.copy_in("copy x (nope) from stdin").err().unwrap();
    assert_eq!(e.code().map(|c| c.code()), Some("42703"));
    let mut w = c.copy_in("copy x from stdin").unwrap();
    w.write_all(b"1\n2\n").unwrap();
    assert_eq!(w.finish().unwrap(), 2);
    assert_eq!(c.query_one("select count(*) from x", &[]).unwrap().get::<_, i64>(0), 2);
}

/// A COPY holds its world's writes while it reads, so a client that goes silent mid-COPY is
/// dropped after CHRONOS_COPY_IDLE: its rows are undone and other writers go on.
#[test]
fn a_silent_copy_client_is_dropped() {
    let dir = std::env::temp_dir().join(format!("chronos-copy-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = common::serve(
        Command::new(env!("CARGO_BIN_EXE_chronos"))
            .args(["serve", dir.to_str().unwrap()])
            .env("CHRONOS_COPY_IDLE", "1"),
        true,
    );
    let _server = Server(s.child, dir);
    let url = format!("host=127.0.0.1 port={} user=tester dbname=main", s.pg);
    let mut a = Client::connect(&url, NoTls).unwrap();
    a.batch_execute("create table t (id int primary key)").unwrap();
    let mut w = a.copy_in("COPY t FROM STDIN").unwrap();
    w.write_all(b"1\n").unwrap();
    w.flush().unwrap(); // then nothing more
    let t = Instant::now();
    let mut b = Client::connect(&url, NoTls).unwrap();
    b.batch_execute("insert into t values (2)").unwrap(); // waited for the COPY to be dropped
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    let n: i64 = b.query_one("select count(*) from t", &[]).unwrap().get(0);
    assert_eq!(n, 1, "the silent COPY's row was undone");
    drop(w);
}

/// COPY ... FROM STDIN (FORMAT binary), as bulk loaders send it: ints, text, floats, jsonb,
/// pgvector's vectors and NULLs, each field its length and bytes.
#[test]
fn driver_copy_in_binary() {
    let (_server, port) = start("binary");
    let mut c = Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main"), NoTls).unwrap();
    c.batch_execute("create table t (id int primary key, name text, v float8, meta jsonb, e vector(3))").unwrap();
    let field = |d: &mut Vec<u8>, b: &[u8]| {
        d.extend((b.len() as i32).to_be_bytes());
        d.extend(b);
    };
    let row = |id: i32, id_bytes: &[u8]| {
        let mut d = 5i16.to_be_bytes().to_vec();
        field(&mut d, id_bytes);
        field(&mut d, format!("n{id}").as_bytes());
        if id == 2 {
            d.extend((-1i32).to_be_bytes()); // NULL
        } else {
            field(&mut d, &(id as f64 / 4.0).to_be_bytes());
        }
        field(&mut d, &[&[1u8][..], format!("{{\"k\": {id}}}").as_bytes()].concat());
        let mut v = vec![0, 3, 0, 0];
        for x in [id as f32, 0.5, -1.0] {
            v.extend(x.to_be_bytes());
        }
        field(&mut d, &v);
        d
    };
    let head = [&b"PGCOPY\n\xff\r\n\0"[..], &[0u8; 8]].concat(); // no flags, no extension
    let mut data = head.clone();
    for id in 1..=3i32 {
        data.extend(row(id, &id.to_be_bytes()));
    }
    data.extend((-1i16).to_be_bytes());
    let mut w = c.copy_in("COPY t FROM STDIN (FORMAT binary)").unwrap();
    for piece in data.chunks(7) {
        w.write_all(piece).unwrap(); // records split across CopyData messages
    }
    assert_eq!(w.finish().unwrap(), 3);
    let mut out = String::new();
    c.copy_out("COPY t TO STDOUT").unwrap().read_to_string(&mut out).unwrap();
    assert_eq!(
        out,
        "1\tn1\t0.25\t{\"k\": 1}\t[1,0.5,-1]\n2\tn2\t\\N\t{\"k\": 2}\t[2,0.5,-1]\n3\tn3\t0.75\t{\"k\": 3}\t[3,0.5,-1]\n"
    );
    // a wrong field count, a cut-off row, a wrong-sized value, a bad signature: errors, nothing goes in
    let mut bad = |data: &[u8]| {
        let mut w = c.copy_in("COPY t FROM STDIN (FORMAT binary)").unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap_err().code().map(|c| c.code().to_string())
    };
    assert_eq!(bad(&[&head[..], &1i16.to_be_bytes(), &[0, 0, 0, 0]].concat()).as_deref(), Some("22P04"));
    assert_eq!(bad(&[head.clone(), row(4, &4i32.to_be_bytes())[..20].to_vec()].concat()).as_deref(), Some("22P04"));
    assert_eq!(bad(&[head.clone(), row(4, &[0, 0, 4])].concat()).as_deref(), Some("22P03"));
    assert_eq!(bad(b"PGCOPY\n\xff\r\n\x01rest of it").as_deref(), Some("22P04"));
    let n: i64 = c.query_one("select count(*) from t", &[]).unwrap().get(0);
    assert_eq!(n, 3);
}

/// psql scripts with COPY data inline, run on Chronos and on Postgres: the same output.
#[test]
fn same_as_postgres() {
    if !have_psql() {
        return;
    }
    let (_server, port) = start("same");
    let Some(pg) = reference("copy_same") else { return };
    let scripts = [
        // text: escapes, \N, a custom delimiter and null; out in text, csv and with options
        "create table t (id int primary key, a text, b double precision, c boolean, d date);
copy t from stdin;
1\tx\\ty\t1.5\tt\t2024-01-02
2\t\\N\t\\N\tf\t\\N
3\tback\\\\slash\\nnew\t-0.25\ttrue\t2020-02-29
4\t\\x41\\102C\t1e20\tno\t1999-12-31
\\.
copy t (id, a) from stdin (delimiter '|', null 'NULL');
5|NULL
6|p|q
\\.
copy t (id, a) from stdin (delimiter '|', null 'NULL');
6|NULL
7|x
\\.
copy t to stdout;
copy t to stdout (format csv, header);
copy t to stdout (format csv, delimiter ';', null 'NULL', quote '''', force_quote (a));
copy t (a, id) to stdout with csv header force quote *;
copy (select id, a || '!' as e from t where id > 2 order by id desc) to stdout (format text, header true);
copy t to stdout (format csv, escape '\\');",
        // csv in: header, quotes, empty vs null, FORCE_NOT_NULL and FORCE_NULL, \r\n
        "create table t (id int primary key, a text, b text);
copy t from stdin (format csv, header);
id,a,b
1,\"x, \"\"y\"\"\",
2,,\"\"
3,\"multi
line\",z
\\.
copy t from stdin (format csv, force_not_null (a), force_null (b));
4,,\"\"
\\.
copy t from stdin with csv quote '''' escape '\\';
5,'it\\'s',x
\\.
copy t to stdout csv;
select id, a is null, b is null from t order by id;",
        // errors: a bad value, missing and extra data, an unknown column, bad options
        "create table t (id int primary key, n int);
copy t from stdin csv;
1,1
2,two
\\.
copy t from stdin csv;
3
\\.
copy t from stdin csv;
3,3,3
\\.
copy t (nope) to stdout;
copy t to stdout (format csv, delimiter ',,');
copy t to stdout (quote 'x');
copy t to stdout (format csv, force_not_null (n));
copy t from stdin (format bogus);
select count(*) from t;",
    ];
    for (i, script) in scripts.iter().enumerate() {
        let run = |to: &To| {
            let (_, out, err) = psql(to, &["-A", "-f", "-"], script.as_bytes());
            format!("{}{err}", String::from_utf8_lossy(&out))
        };
        let (a, b) = (run(&To::Chronos(port)), run(&pg));
        assert_eq!(a, b, "script {i} differs: chronos first, then postgres");
        psql(&To::Chronos(port), &["-c", "drop table t"], b"");
        psql(&pg, &["-c", "drop table t"], b"");
    }
    if let To::Postgres(schema) = pg {
        psql(&pg, &["-c", &format!("drop schema {schema} cascade")], b"");
    }
}

/// COPY (INSERT | UPDATE | DELETE ... RETURNING ...) TO sends the rows the statement returns. Without a
/// RETURNING it is refused, and so is what its table's rules would make of it, each as Postgres says.
#[test]
fn copy_of_a_statement_that_changes_rows() {
    if !have_psql() {
        return;
    }
    let (_server, port) = start("dml");
    let Some(pg) = reference("copy_dml") else { return };
    let script = "create table t (id int primary key, a text);
copy (insert into t values (1, 'x'), (2, 'y') returning id, a) to stdout;
copy (update t set a = a || '!' where id = 2 returning *) to stdout (format csv, header);
copy (delete from t where id = 1 returning a) to stdout;
copy (insert into t values (3, 'z')) to stdout;
copy (update t set a = 'q') to stdout;
copy (delete from t) to stdout;
create rule r as on insert to t do instead nothing;
copy (insert into t values (4, 'w')) to stdout;
copy (insert into t values (4, 'w') returning id) to stdout;
drop rule r on t;
create rule r as on insert to t do also delete from t where id = 0;
copy (insert into t values (4, 'w')) to stdout;
copy (insert into t values (4, 'w') returning id) to stdout;
drop rule r on t;
create rule r as on insert to t where new.a <> 'f' do instead delete from t where id = 0;
copy (insert into t values (4, 'w')) to stdout;
copy (insert into t values (4, 'w') returning id) to stdout;
drop rule r on t;
create rule r as on insert to t do instead (delete from t where id = 0; delete from t where id = 0);
copy (insert into t values (4, 'w')) to stdout;
copy (insert into t values (4, 'w') returning id) to stdout;
drop rule r on t;
create rule r as on insert to t do instead notify t;
copy (insert into t values (4, 'w')) to stdout;
copy (insert into t values (4, 'w') returning id) to stdout;
drop rule r on t;
select * from t order by id;";
    let run = |to: &To| {
        let (_, out, err) = psql(to, &["-A", "-f", "-"], script.as_bytes());
        format!("{}{err}", String::from_utf8_lossy(&out))
    };
    let (a, b) = (run(&To::Chronos(port)), run(&pg));
    assert_eq!(a, b, "chronos first, then postgres");
    psql(&pg, &["-c", "drop table t"], b"");
    if let To::Postgres(schema) = pg {
        psql(&pg, &["-c", &format!("drop schema {schema} cascade")], b"");
    }
}

/// A READ ONLY transaction may still write temporary tables, as Postgres lets it: by COPY FROM and
/// by a data-changing WITH too; a WITH or a COPY of a statement that writes another table is
/// refused (25006), named by its top-level command. CLUSTER, REINDEX and LOCK TABLE run there, as
/// in Postgres (the stronger modes too: pgdiff's `read_only_transactions_run_maintenance_...`).
#[test]
fn read_only_transactions_write_temporary_tables_only() {
    if !have_psql() {
        return;
    }
    let (_server, port) = start("readonly");
    let Some(pg) = reference("copy_readonly") else { return };
    let script = "create table ro_perm (a int primary key, b int);
create index ro_perm_b on ro_perm (b);
insert into ro_perm values (1, 2), (2, 1);
create temp table ro_tmp (a int);
begin read only;
copy ro_tmp from stdin;
1
2
\\.
with x as (insert into ro_tmp values (5) returning a) select * from x;
with x as (update ro_tmp set a = a + 10 where a = 1 returning a) select * from x;
select * from ro_tmp order by a;
cluster ro_perm using ro_perm_b;
reindex table ro_perm;
lock table ro_perm in access share mode;
lock table ro_perm in row exclusive mode;
commit;
select count(*) from ro_tmp;
begin read only;
with x as (insert into ro_perm values (5, 5) returning a) select * from x;
rollback;
begin read only;
with x as (select 1) insert into ro_perm select 6, 6 from x;
rollback;
begin read only;
copy (insert into ro_perm values (7, 7) returning a) to stdout;
rollback;
select * from ro_perm order by a;
drop table ro_perm;";
    let run = |to: &To| {
        let (_, out, err) = psql(to, &["-A", "-f", "-"], script.as_bytes());
        format!("{}{err}", String::from_utf8_lossy(&out))
    };
    let (a, b) = (run(&To::Chronos(port)), run(&pg));
    assert_eq!(a, b, "chronos first, then postgres");
    if let To::Postgres(schema) = pg {
        psql(&pg, &["-c", &format!("drop schema {schema} cascade")], b"");
    }
}

/// A COPY whose rows take sequence values as they are stored (a trigger's nextval, a default worked
/// out for each row) runs out of the values reserved for it and is run again, more than once here:
/// every row is stored, and the sequence skips no value. It used to say COPY n and store nothing.
#[test]
fn copy_that_takes_sequence_values_stores_every_row() {
    if !have_psql() {
        return;
    }
    let (_server, port) = start("seqcopy");
    let Some(pg) = reference("copy_seq") else { return };
    let ids = |n: usize| (1..=n).map(|i| format!("{i}\n")).collect::<String>();
    let script = format!(
        "create sequence cs;
create table t (id int primary key, n int);
create function setn() returns trigger language plpgsql as $$ begin new.n := nextval('cs') * 10; return new; end $$;
create trigger t_n before insert on t for each row execute function setn();
copy t (id) from stdin;
{}\\.
select count(*), min(n), max(n), count(distinct n) from t;
select nextval('cs');
create table u (id int primary key, seen bigint default lastval(), n int default nextval('cs') + 1000, r double precision default random());
copy u (id) from stdin;
{}\\.
select count(*), min(n), max(n), count(distinct r), min(seen), count(distinct seen) from u;
insert into t (id) values (1000), (1001);
select id, n from t where id >= 1000 order by id;",
        ids(300),
        ids(50)
    );
    let run = |to: &To| {
        let (_, out, err) = psql(to, &["-A", "-f", "-"], script.as_bytes());
        format!("{}{err}", String::from_utf8_lossy(&out))
    };
    let (a, b) = (run(&To::Chronos(port)), run(&pg));
    assert_eq!(a, b, "chronos first, then postgres");
    assert!(a.contains("300|10|3000|300"), "{a}");
    psql(&pg, &["-c", "drop table t"], b"");
    if let To::Postgres(schema) = pg {
        psql(&pg, &["-c", &format!("drop schema {schema} cascade")], b"");
    }
}

/// A COPY fails whole, or stores every row: an error working out its WHERE fails it (the row was counted as
/// left out); FORCE_QUOTE of a column a data-changing statement doesn't return is found before the statement
/// runs (its rows stayed stored); and rows that first take a sequence's value past the first part are kept to
/// be stored again (the COPY was refused).
#[test]
fn copy_fails_whole_or_stores_every_row() {
    if !have_psql() {
        return;
    }
    let (_server, port) = start("whole");
    let Some(pg) = reference("copy_whole") else { return };
    let ids = |n: usize| (1..=n).map(|i| format!("{i}\n")).collect::<String>();
    let script = format!(
        "create table w (id int primary key, v int);
copy w from stdin where 10 / v > 0;
1\t5
2\t0
3\t1
\\.
select count(*) from w;
create table f (id int primary key);
copy (insert into f values (1), (2) returning id) to stdout (format csv, force_quote (missing));
select count(*) from f;
copy (insert into f values (1), (2) returning id) to stdout (format csv, force_quote (id));
create sequence late;
create table l (id int primary key, n int);
create function ln() returns trigger language plpgsql as $$ begin if new.id > 20000 then new.n := nextval('late'); end if; return new; end $$;
create trigger l_n before insert on l for each row execute function ln();
copy l (id) from stdin;
{}\\.
select count(*), count(n), min(n), max(n) from l;",
        ids(20100)
    );
    let run = |to: &To| {
        let (_, out, err) = psql(to, &["-q", "-A", "-t", "-f", "-"], script.as_bytes());
        (String::from_utf8_lossy(&out).into_owned(), err)
    };
    let ((a, a_err), (b, b_err)) = (run(&To::Chronos(port)), run(&pg));
    assert_eq!((&a, &a_err), (&b, &b_err), "chronos first, then postgres");
    assert_eq!(a.lines().collect::<Vec<_>>(), ["0", "0", "\"1\"", "\"2\"", "20100|100|1|100"], "{a_err}");
    // (division by zero in the WHERE, then the column FORCE_QUOTE names that the statement doesn't return)
    assert!(a_err.contains("22012") && a_err.contains("42703"), "{a_err}");
    psql(&pg, &["-c", "drop table w, f, l"], b"");
    if let To::Postgres(schema) = pg {
        psql(&pg, &["-c", &format!("drop schema {schema} cascade")], b"");
    }
}

/// A generated column is left out of a COPY with no column list, and refused when it is named.
#[test]
fn generated_columns_stay_out_of_copy() {
    if !have_psql() {
        return;
    }
    let (_server, port) = start("generated");
    let Some(pg) = reference("copy_generated") else { return };
    let script = "create table t (id int primary key, x int, y int generated always as (x * 2) stored);
copy t (id, x) from stdin;
1\t5
2\t\\N
\\.
copy t from stdin;
3\t7
\\.
copy t (id, x, y) from stdin;
select * from t order by id;
copy t to stdout;
copy t (id, y) to stdout;
copy (select * from t order by id) to stdout;";
    let run = |to: &To| {
        let (_, out, err) = psql(to, &["-A", "-f", "-"], script.as_bytes());
        format!("{}{err}", String::from_utf8_lossy(&out))
    };
    let (a, b) = (run(&To::Chronos(port)), run(&pg));
    assert_eq!(a, b, "chronos first, then postgres");
    psql(&pg, &["-c", "drop table t"], b"");
    if let To::Postgres(schema) = pg {
        psql(&pg, &["-c", &format!("drop schema {schema} cascade")], b"");
    }
}

/// COPY's rows per second against multi-row INSERTs of 1,000 rows, in process (no network), on
/// the same 100,000 rows.
#[test]
fn copy_speed() {
    let db = chronos::Db::new();
    let mut s = Session::new(chronos::MAIN);
    let make = "create table t (id int primary key, name text, score double precision)";
    let n = 100_000;
    let mut csv = String::new();
    let mut inserts = vec![];
    let mut batch = vec![];
    for i in 1..=n {
        csv += &format!("{i},name {i},{}\n", i as f64 / 8.0);
        batch.push(format!("({i}, 'name {i}', {})", i as f64 / 8.0));
        if batch.len() == 1000 {
            inserts.push(format!("insert into t2 values {}", batch.join(", ")));
            batch.clear();
        }
    }
    s.run(&db, make, &[]).unwrap();
    s.run(&db, &make.replace(" t ", " t2 "), &[]).unwrap();
    let t = Instant::now();
    let stmt = chronos::sql::parse("copy t from stdin csv").unwrap().pop().unwrap();
    let copy = s.copy_in(&db, &stmt).unwrap();
    let mut pieces: Vec<Vec<u8>> = csv.as_bytes().chunks(65536).map(<[u8]>::to_vec).rev().collect();
    assert_eq!(copy.run(&mut s, &db, Box::new(move || Ok(pieces.pop()))).unwrap().tag, format!("COPY {n}"));
    let copy_rate = n as f64 / t.elapsed().as_secs_f64();
    let t = Instant::now();
    for q in &inserts {
        s.run(&db, q, &[]).unwrap();
    }
    let insert_rate = n as f64 / t.elapsed().as_secs_f64();
    eprintln!("COPY: {copy_rate:.0} rows/s; INSERT of 1,000 rows at a time: {insert_rate:.0} rows/s");
    let mut count =
        |t: &str| s.run(&db, &format!("select count(*), sum(score) from {t}"), &[]).unwrap().pop().unwrap().rows;
    assert_eq!(count("t"), count("t2"));
}

/// COPY fires INSERT triggers as INSERT does: row triggers per row, statement triggers once
/// (the rows past one batch included).
#[test]
fn copy_fires_insert_triggers() {
    let db = chronos::Db::new();
    let mut s = Session::new(chronos::MAIN);
    s.run(
        &db,
        "create table t (id int primary key, v int);
         create table log (n serial primary key, what text);
         create function logrow() returns trigger language plpgsql as $$ begin new.v := new.v * 2; insert into log (what) values ('row'); return new; end $$;
         create function logstmt() returns trigger language plpgsql as $$ begin insert into log (what) values ('stmt'); return null; end $$;
         create trigger r before insert on t for each row execute function logrow();
         create trigger st after insert on t for each statement execute function logstmt();",
        &[],
    )
    .unwrap();
    let data: String = (1..=25_000).map(|i| format!("{i},{i}\n")).collect();
    let stmt = chronos::sql::parse("copy t from stdin csv").unwrap().pop().unwrap();
    let copy = s.copy_in(&db, &stmt).unwrap();
    let mut pieces: Vec<Vec<u8>> = data.as_bytes().chunks(8192).map(<[u8]>::to_vec).rev().collect();
    copy.run(&mut s, &db, Box::new(move || Ok(pieces.pop()))).unwrap();
    let out = s.run(&db, "select what, count(*) from log group by what order by what", &[]).unwrap().pop().unwrap();
    let got: Vec<String> = out.rows.iter().map(|r| format!("{}={}", r[0].to_text(), r[1].to_text())).collect();
    assert_eq!(got, ["row=25000", "stmt=1"]);
    let sum = s.run(&db, "select sum(v) from t", &[]).unwrap().pop().unwrap();
    assert_eq!(sum.rows[0][0].to_text(), (25_000i64 * 25_001).to_string(), "BEFORE ROW changed each row");
}

/// The server's own files, where the server lets anyone at them (CHRONOS_ALLOW_SERVER_COPY=1), go
/// as Postgres guards them: COPY FROM a file takes the privileges of pg_read_server_files and COPY
/// TO one those of pg_write_server_files (a superuser has both), refused with Postgres's error before
/// any file is opened or query run; lo_import and lo_export are a superuser's; making a file_fdw
/// table takes pg_read_server_files, and here reading one does too. An agent's session never may,
/// whatever its role or its login's identity.
#[test]
fn server_files_take_postgres_privileges() {
    let files = std::env::temp_dir().join(format!("chronos-copy-{}-files", std::process::id()));
    let _ = std::fs::remove_dir_all(&files);
    std::fs::create_dir_all(&files).unwrap();
    let dir = std::env::temp_dir().join(format!("chronos-copy-{}-server-files", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]).env("CHRONOS_ALLOW_SERVER_COPY", "1");
    let s = common::serve(&mut cmd, true);
    let _server = Server(s.child, dir);
    let port = s.pg;
    let conn = |user: &str, password: Option<&str>| {
        let pw = password.map(|p| format!(" password={p}")).unwrap_or_default();
        Client::connect(&format!("host=127.0.0.1 port={port} user={user} dbname=main{pw}"), NoTls).unwrap()
    };
    let path = |name: &str| files.join(name).to_str().unwrap().to_string();
    let rows = path("rows.csv");
    let mut admin = conn("tester", None);
    admin.batch_execute("create table t (id int, v text); insert into t values (1, 'a'), (2, 'b')").unwrap();
    // the database's own user (a login that is no role: the bootstrap superuser) may
    admin.batch_execute(&format!("copy t to '{rows}' with (format csv)")).unwrap();
    assert_eq!(std::fs::read_to_string(&rows).unwrap(), "1,a\n2,b\n");
    admin.batch_execute(&format!("copy t from '{rows}' with (format csv)")).unwrap();
    let count = |c: &mut Client, q: &str| c.query_one(q, &[]).unwrap().get::<_, i64>(0);
    assert_eq!(count(&mut admin, "select count(*) from t"), 4);
    admin
        .batch_execute(
            "create role sf_none login; create role sf_read login; create role sf_write login;
             grant pg_read_server_files to sf_read; grant pg_write_server_files to sf_write;
             grant select, insert on t to sf_none, sf_read, sf_write;
             grant create on schema public to sf_none, sf_read",
        )
        .unwrap();
    let refused = |c: &mut Client, q: &str| {
        let e = c.batch_execute(q).unwrap_err();
        let d = e.as_db_error().unwrap_or_else(|| panic!("{q}: {e}"));
        (d.code().code().to_string(), d.message().to_string(), d.detail().map(String::from))
    };
    let denied = |what: &str, role: &str| {
        (
            "42501".to_string(),
            format!("permission denied to COPY {what} a file"),
            Some(format!("Only roles with privileges of the \"{role}\" role may COPY {what} a file.")),
        )
    };
    let (from, to) = (denied("from", "pg_read_server_files"), denied("to", "pg_write_server_files"));
    let mut none = conn("sf_none", None);
    let out = path("out.csv");
    assert_eq!(refused(&mut none, &format!("copy t from '{rows}'")), from);
    assert_eq!(refused(&mut none, &format!("copy t to '{out}'")), to);
    assert_eq!(refused(&mut none, &format!("copy (insert into t values (9) returning id) to '{out}'")), to);
    assert!(!std::path::Path::new(&out).exists());
    assert_eq!(count(&mut admin, "select count(*) from t where id = 9"), 0, "refused before it ran");
    let mut read = conn("sf_read", None);
    read.batch_execute(&format!("copy t from '{rows}' with (format csv)")).unwrap();
    assert_eq!(refused(&mut read, &format!("copy t to '{out}'")), to);
    let mut write = conn("sf_write", None);
    write.batch_execute(&format!("copy t to '{out}' with (format csv)")).unwrap();
    assert!(std::path::Path::new(&out).exists());
    assert_eq!(refused(&mut write, &format!("copy t from '{rows}'")), from);
    // lo_import and lo_export: a superuser's only
    admin.batch_execute(&format!("select lo_import('{rows}')")).unwrap();
    let lo = refused(&mut read, &format!("select lo_import('{rows}')"));
    assert_eq!((lo.0.as_str(), lo.1.as_str()), ("42501", "permission denied for function lo_import"));
    // file_fdw's filename: pg_read_server_files to make one, and here to read one
    let ft = |name: &str| {
        format!("create foreign table {name} (id int, v text) server files options (filename '{rows}', format 'csv')")
    };
    let e = refused(&mut none, &ft("ft_none"));
    assert_eq!(e.1, "permission denied to set the \"filename\" option of a file_fdw foreign table");
    assert_eq!(
        e.2.as_deref(),
        Some("Only roles with privileges of the \"pg_read_server_files\" role may set this option.")
    );
    read.batch_execute(&ft("ft")).unwrap();
    assert_eq!(count(&mut read, "select count(*) from ft"), 2);
    read.batch_execute("grant select on ft to sf_none").unwrap();
    assert_eq!(refused(&mut none, "select * from ft").0, "42501");
    // an agent: never
    let token: String = admin.query_one("create agent sf_bot", &[]).unwrap().get("token");
    let mut bot = conn("sf_bot", Some(&token));
    assert_eq!(refused(&mut bot, &format!("copy t to '{}'", path("bot.csv"))).0, "42501");
    assert_eq!(refused(&mut bot, &format!("copy t from '{rows}'")).0, "42501");
    assert_eq!(refused(&mut bot, &format!("select lo_import('{rows}')")).0, "42501");
    assert_eq!(refused(&mut bot, "select * from ft").0, "42501");
    assert!(!files.join("bot.csv").exists());
    let _ = std::fs::remove_dir_all(&files);
}
