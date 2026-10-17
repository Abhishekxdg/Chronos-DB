//! Fuzz smoke tests: the SQL parser and executor, and the Postgres wire protocol, fed input no client
//! means to send. Nothing here checks an answer; a statement or a frame may fail any way it likes,
//! as long as nothing panics, hangs or stops the server answering the next connection. The inputs come
//! from a seeded generator, so a failure names a seed and a statement that give it again.
//!
//!     CHRONOS_FUZZ_SEED=7 CHRONOS_FUZZ_ROUNDS=200000 cargo test --release --test fuzz

mod common;

use chronos::Db;
use chronos::sql::Session;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::Duration;

/// xorshift64*: small, seeded, the same on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of[self.below(of.len())]
    }
}

fn seed() -> u64 {
    std::env::var("CHRONOS_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5EED_C0DE)
}

fn rounds(default: usize) -> usize {
    std::env::var("CHRONOS_FUZZ_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

const WORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "create",
    "table",
    "index",
    "drop",
    "alter",
    "add",
    "column",
    "primary",
    "key",
    "references",
    "unique",
    "check",
    "default",
    "not",
    "null",
    "and",
    "or",
    "in",
    "exists",
    "between",
    "like",
    "is",
    "case",
    "when",
    "then",
    "else",
    "end",
    "join",
    "left",
    "on",
    "using",
    "group",
    "by",
    "order",
    "having",
    "limit",
    "offset",
    "union",
    "all",
    "distinct",
    "as",
    "with",
    "recursive",
    "returning",
    "begin",
    "commit",
    "rollback",
    "savepoint",
    "t",
    "u",
    "a",
    "b",
    "c",
    "id",
    "v",
    "int",
    "text",
    "numeric",
    "jsonb",
    "boolean",
    "date",
    "timestamp",
    "hstore",
    "ltree",
    "citext",
    "count",
    "sum",
    "min",
    "max",
    "coalesce",
    "nullif",
    "lower",
    "upper",
    "length",
    "now",
    "true",
    "false",
    "over",
    "partition",
    "window",
    "filter",
    "array",
    "row",
    "cast",
    "collate",
    "explain",
    "copy",
    "to",
    "stdout",
    "merge",
    "view",
    "function",
    "returns",
    "language",
    "sql",
    "plpgsql",
    "trigger",
    "generated",
    "always",
    "identity",
    "conflict",
    "do",
    "nothing",
    "lateral",
    "unnest",
    "any",
    "some",
    "interval",
    "extract",
    "nlevel",
    "akeys",
    "each",
];

const SYMBOLS: &[&str] = &[
    "(", ")", ",", ";", "*", "+", "-", "/", "%", "=", "<>", "<", ">", "<=", ">=", "||", "->", "->>", "@>", "<@", "?",
    "::", ".", "[", "]", "'x'", "'a=>1'", "'a.b'", "''", "'", "\"", "\"t\"", "$1", "$$", "1", "0", "-1", "42", "3.14",
    "1e3", "null", "--", "/*", "*/", "\\", "é", "\n", "~", "!", "&", "|", "#", "@", ":=", "=>", "{", "}",
];

/// Statements that parse and run on the tables below: what the mutations start from.
const SEEDS: &[&str] = &[
    "select id, v from t where id between 1 and 3 order by v desc limit 2",
    "select t.id, count(*) from t join u on u.t_id = t.id group by t.id having count(*) > 0",
    "insert into t (id, v) values (9, 'nine') on conflict (id) do update set v = excluded.v returning id",
    "update t set v = v || '!' where id in (select t_id from u) returning *",
    "delete from u where t_id = any (array[1, 2]) returning id",
    "with recursive r(n) as (select 1 union all select n + 1 from r where n < 5) select sum(n) from r",
    "select case when v like 'a%' then 1 else 0 end, coalesce(nullif(v, ''), 'none') from t",
    "select id, row_number() over (partition by v order by id) from t",
    "create table z (id int primary key, h hstore default '', p ltree, c citext unique, n numeric(6,2) check (n >= 0))",
    "select h -> 'a', p <@ 'a.b', nlevel(p), akeys(h) from z where h ? 'a' or p ~ '*.b'",
    "alter table t add column extra jsonb default '{}'",
    "create index t_v on t (lower(v))",
    "select * from t, lateral (select count(*) from u where u.t_id = t.id) s",
    "select extract(year from now()), now() + interval '1 day', '2026-01-01'::date - 1",
    "begin; insert into t values (20, 'tx'); rollback",
    "create view tv as select id from t where v is not null",
    "select '{\"a\": [1, 2]}'::jsonb -> 'a' ->> 0, jsonb_build_object('k', id) from t",
    "explain select * from t where id = 1",
];

fn fresh() -> (Db, Session) {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create table t (id int primary key, v text)",
        "create table u (id serial primary key, t_id int references t (id), w numeric)",
        "insert into t values (1, 'a'), (2, 'b'), (3, null)",
        "insert into u (t_id, w) values (1, 1.5), (1, 2), (2, null)",
        // (a statement that runs away is cut off: nothing here should take long)
        "set statement_timeout = 2000",
    ] {
        s.run(&db, q, &[]).unwrap();
    }
    (db, s)
}

/// Tokens thrown together, or a statement that runs with some of its tokens dropped, doubled, swapped or replaced.
fn statement(r: &mut Rng) -> String {
    let any = |r: &mut Rng| if r.below(3) == 0 { r.pick(SYMBOLS) } else { r.pick(WORDS) };
    if r.below(2) == 0 {
        let n = 1 + r.below(30);
        return (0..n).map(|_| any(r)).collect::<Vec<_>>().join(" ");
    }
    let mut toks: Vec<&str> = r.pick(SEEDS).split(' ').collect();
    for _ in 0..1 + r.below(4) {
        let at = r.below(toks.len());
        match r.below(5) {
            0 => drop(toks.remove(at)),
            1 => toks.insert(at, toks[at]),
            2 => {
                let other = r.below(toks.len());
                toks.swap(at, other);
            }
            3 => toks[at] = any(r),
            _ => toks.insert(at, any(r)),
        }
        if toks.is_empty() {
            toks.push("select");
        }
    }
    toks.join(" ")
}

/// Statements nobody would write: each is refused or runs, and none panics. A fresh database every so often, so
/// that what one statement left behind (a dropped table, an open transaction) doesn't make the rest all fail alike.
#[test]
fn sql_that_makes_no_sense_never_panics() {
    let mut r = Rng(seed());
    let (mut db, mut s) = fresh();
    for round in 0..rounds(6000) {
        if round % 200 == 0 {
            (db, s) = fresh();
        }
        let q = statement(&mut r);
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = s.run(&db, &q, &[]);
        }));
        assert!(ran.is_ok(), "seed {}, round {round}: panicked on\n{q}", seed());
    }
}

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

/// A startup message a server accepts: protocol 3.0, a user and a database.
fn startup() -> Vec<u8> {
    let mut body = 196_608i32.to_be_bytes().to_vec();
    body.extend(b"user\0fuzz\0database\0main\0\0");
    let mut out = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    out.extend(body);
    out
}

/// One frame: a type the protocol has (or not), a length that is right, short, long or absurd, and bytes.
fn frame(r: &mut Rng) -> Vec<u8> {
    const TYPES: &[u8] = b"QPBEDSCHXdcfp\0\xffZ";
    let mut body: Vec<u8> = (0..r.below(64)).map(|_| r.next() as u8).collect();
    // (often a statement's text, or a name and a count, so that some frames get past the first check)
    match r.below(4) {
        0 => body = [b"select 1;\0".to_vec(), body].concat(),
        1 => body = [b"s\0select $1\0".to_vec(), (r.below(5) as i16).to_be_bytes().to_vec(), body].concat(),
        _ => {}
    }
    let len: i32 = match r.below(8) {
        0 => 0,
        1 => 3,
        2 => -1,
        3 => i32::MAX,
        4 => (body.len() + 4 + 1 + r.below(1000)) as i32,
        5 => (body.len() / 2) as i32,
        _ => (body.len() + 4) as i32,
    };
    let mut out = vec![TYPES[r.below(TYPES.len())]];
    out.extend(len.to_be_bytes());
    out.extend(body);
    out
}

/// Connections that send frames no driver would: a wrong length, an unknown type, a message cut short, bytes
/// instead of a startup. The server may close each one; it must go on answering a real client after them.
#[test]
fn frames_that_make_no_sense_never_stop_the_server() {
    let dir = std::env::temp_dir().join(format!("chronos-fuzz-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    let served = common::serve(&mut cmd, true);
    let (_server, port) = (Server(served.child, dir), served.pg);
    let answers = |what: &str| {
        let mut c =
            postgres::Client::connect(&format!("host=127.0.0.1 port={port} user=fuzz dbname=main"), postgres::NoTls)
                .unwrap_or_else(|e| panic!("seed {}: no connection {what}: {e}", seed()));
        let one: i32 = c.query_one("select 1", &[]).unwrap_or_else(|e| panic!("seed {}: {what}: {e}", seed())).get(0);
        assert_eq!(one, 1);
    };
    answers("before any frame");
    let mut r = Rng(seed() ^ 0xF0F0);
    for round in 0..rounds(400) {
        let Ok(mut c) = TcpStream::connect(("127.0.0.1", port)) else {
            panic!("seed {}, round {round}: the server stopped listening", seed());
        };
        c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        c.set_write_timeout(Some(Duration::from_millis(500))).unwrap();
        // most connections start as a client does, then go wrong; some are wrong from the first byte
        if r.below(5) != 0 {
            let _ = c.write_all(&startup());
        }
        for _ in 0..1 + r.below(6) {
            if c.write_all(&frame(&mut r)).is_err() {
                break; // (closed on us: what a server does with a frame it can't read)
            }
        }
        // some hang up at once; some read what came back first; some stop mid-frame
        match r.below(3) {
            0 => {}
            1 => {
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf);
            }
            _ => {
                let _ = c.write_all(&[b'Q', 0, 0]);
            }
        }
        drop(c);
        if round % 50 == 49 {
            answers(&format!("after round {round}"));
        }
    }
    answers("after every frame");
}
