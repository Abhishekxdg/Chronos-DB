//! pgcrypto where its answers are random, so `pgdiff.rs` can't compare them: a password hash or an
//! OpenPGP message that Chronos makes, Postgres reads, and the other way round, each over the Postgres
//! protocol through the `postgres` crate. Needs the Postgres at host=/tmp (port PGPORT) with pgcrypto;
//! those tests are skipped without one, unless CHRONOS_REQUIRE_ORACLE is set.
//!
//! The rest needs no Postgres: the ciphers Postgres on OpenSSL 3 refuses unless OpenSSL's legacy provider
//! is loaded (bf, cast5, des), checked against what pgcrypto gives with it loaded; messages gpg made; and
//! CREATE and DROP EXTENSION, which keep pgcrypto in the world.

mod common;

use chronos::sql::{Session, Value, take_notices};
use chronos::{Db, MAIN};
use postgres::{Client, NoTls};
use std::process::Child;

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

/// A Chronos server, and a client of it.
fn chronos(name: &str) -> (Server, Client) {
    let dir = std::env::temp_dir().join(format!("chronos-pgcrypto-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    let s = common::serve(&mut cmd, true);
    let c = Client::connect(&format!("host=127.0.0.1 port={} user=tester dbname=main", s.pg), NoTls).unwrap();
    (Server(s.child, dir), c)
}

/// The reference Postgres, with pgcrypto (in public: never dropped here); None when there's none, unless
/// CHRONOS_REQUIRE_ORACLE says there must be.
fn oracle() -> Option<Client> {
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    match Client::connect(&format!("host=/tmp port={port} dbname=postgres"), NoTls) {
        Ok(mut c) => {
            c.batch_execute("create extension if not exists pgcrypto").unwrap();
            Some(c)
        }
        Err(e) => {
            assert!(
                std::env::var_os("CHRONOS_REQUIRE_ORACLE").is_none(),
                "CHRONOS_REQUIRE_ORACLE is set but there is no Postgres at host=/tmp port={port}: {e}"
            );
            eprintln!("skipped: no Postgres at host=/tmp port={port}");
            None
        }
    }
}

fn text(c: &mut Client, sql: &str, args: &[&(dyn postgres::types::ToSql + Sync)]) -> String {
    c.query_one(sql, args).unwrap_or_else(|e| panic!("{sql}: {:?}", e.as_db_error())).get(0)
}

fn bytes(c: &mut Client, sql: &str, args: &[&(dyn postgres::types::ToSql + Sync)]) -> Vec<u8> {
    c.query_one(sql, args).unwrap_or_else(|e| panic!("{sql}: {:?}", e.as_db_error())).get(0)
}

fn yes(c: &mut Client, sql: &str, args: &[&(dyn postgres::types::ToSql + Sync)]) -> bool {
    c.query_one(sql, args).unwrap_or_else(|e| panic!("{sql}: {:?}", e.as_db_error())).get(0)
}

const PASSWORDS: [&str; 4] = ["secret", "", "pässwörd €", "a password much longer than eight or sixteen characters"];

#[test]
fn password_hashes_both_ways() {
    let Some(mut pg) = oracle() else { return };
    let (_server, mut ch) = chronos("crypt");
    for salt in ["gen_salt('bf', 4)", "gen_salt('bf')", "gen_salt('md5')", "gen_salt('xdes', 1001)", "gen_salt('des')"]
    {
        for pw in PASSWORDS {
            let sql = format!("select crypt($1::text, {salt})");
            let check = "select crypt($1::text, $2::text) = $2::text";
            let mine = text(&mut ch, &sql, &[&pw]);
            assert!(yes(&mut pg, check, &[&pw, &mine]), "Postgres rejects Chronos's {salt} hash of {pw:?}: {mine}");
            assert!(!yes(&mut pg, check, &[&"wrong", &mine]), "{mine}");
            let theirs = text(&mut pg, &sql, &[&pw]);
            assert!(yes(&mut ch, check, &[&pw, &theirs]), "Chronos rejects Postgres's {salt} hash of {pw:?}: {theirs}");
            assert!(!yes(&mut ch, check, &[&"wrong", &theirs]), "{theirs}");
        }
    }
}

/// Option sets of pgp_sym_encrypt both engines take (bf and cast5 aside: see `legacy_ciphers`).
const PGP_OPTIONS: [&str; 15] = [
    "",
    "cipher-algo=aes256",
    "cipher-algo=aes192, s2k-mode=1",
    "cipher-algo=3des, s2k-mode=0, s2k-digest-algo=md5",
    "compress-algo=1",
    "compress-algo=2, compress-level=9",
    "compress-algo=1, compress-level=0, cipher-algo=aes256",
    "disable-mdc=1",
    "disable-mdc=1, compress-algo=2, cipher-algo=3des",
    "sess-key=1",
    "sess-key=1, s2k-cipher-algo=aes256, cipher-algo=3des",
    "sess-key=0, s2k-cipher-algo=3des",
    "s2k-count=100000, s2k-digest-algo=sha512",
    "s2k-digest-algo=ripemd160, unicode-mode=1",
    "convert-crlf=1, s2k-digest-algo=sha256",
];

#[test]
fn pgp_messages_both_ways() {
    let Some(mut pg) = oracle() else { return };
    let (_server, mut ch) = chronos("pgp");
    let long = "a line of text, said again\n".repeat(4000); // past 8 KiB: lengths in several octets, or partial
    let texts = ["hello world", "", "héllo wörld € \u{1F600}", "two\nlines\n", long.as_str()];
    for opts in PGP_OPTIONS {
        // (with these, pgcrypto writes the S2K's cipher in the session key packet but encrypts the data with the
        // other, so not even Postgres reads its own message: Chronos's uses the cipher the packet names)
        let theirs_readable = opts != "sess-key=0, s2k-cipher-algo=3des";
        for t in texts {
            let enc = "select pgp_sym_encrypt($1::text, $2::text, $3::text)";
            let dec = "select pgp_sym_decrypt($1::bytea, $2::text, $3::text)";
            // (convert-crlf is undone on the way out only when asked)
            let mine = bytes(&mut ch, enc, &[&t, &"pw", &opts]);
            let back = text(&mut pg, dec, &[&mine, &"pw", &opts]);
            assert!(back == t, "Postgres read Chronos's message ({opts}) of {} bytes wrong", t.len());
            let theirs = bytes(&mut pg, enc, &[&t, &"pw", &opts]);
            if !theirs_readable {
                let e = pg.query_one(dec, &[&theirs, &"pw", &opts]).unwrap_err();
                assert_eq!(e.code().map(|c| c.code()), Some("39000"), "Postgres reads its own message ({opts})?");
                continue;
            }
            let back = text(&mut ch, dec, &[&theirs, &"pw", &opts]);
            assert!(back == t, "Chronos read Postgres's message ({opts}) of {} bytes wrong", t.len());
            for (who, c, m) in [("Chronos", &mut ch, &theirs), ("Postgres", &mut pg, &mine)] {
                let e = c.query_one(dec, &[m, &"not the password", &""]).unwrap_err();
                assert_eq!(e.code().map(|c| c.code()), Some("39000"), "{who}, wrong password, {opts}: {e}");
            }
        }
        // bytea, any bytes
        let data: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
        let enc = "select pgp_sym_encrypt_bytea($1::bytea, $2::text, $3::text)";
        let dec = "select pgp_sym_decrypt_bytea($1::bytea, $2::text, $3::text)";
        let mine = bytes(&mut ch, enc, &[&data, &"pw", &opts]);
        assert!(bytes(&mut pg, dec, &[&mine, &"pw", &opts]) == data, "Postgres read Chronos's bytea ({opts}) wrong");
        let theirs = bytes(&mut pg, enc, &[&data, &"pw", &opts]);
        if !theirs_readable {
            continue;
        }
        assert!(bytes(&mut ch, dec, &[&theirs, &"pw", &opts]) == data, "Chronos read Postgres's bytea ({opts}) wrong");
        // a bytea message is no text, either way
        for (who, c, m) in [("Chronos", &mut ch, &theirs), ("Postgres", &mut pg, &mine)] {
            let e = c.query_one("select pgp_sym_decrypt($1::bytea, 'pw')", &[m]).unwrap_err();
            assert_eq!(e.code().map(|c| c.code()), Some("39000"), "{who}: {e}");
        }
    }
    // armored, and the key id
    let mine = text(&mut ch, "select armor(pgp_sym_encrypt('armored', 'pw'))", &[]);
    assert_eq!(text(&mut pg, "select pgp_sym_decrypt(dearmor($1), 'pw')", &[&mine]), "armored");
    let theirs = text(&mut pg, "select armor(pgp_sym_encrypt('armored', 'pw'))", &[]);
    assert_eq!(text(&mut ch, "select pgp_sym_decrypt(dearmor($1), 'pw')", &[&theirs]), "armored");
    assert_eq!(text(&mut ch, "select pgp_key_id(dearmor($1))", &[&theirs]), "SYMKEY");
}

#[test]
fn random_bytes_and_salts() {
    let (_server, mut c) = chronos("random");
    for n in [1i32, 2, 15, 16, 17, 255, 1024] {
        let b = bytes(&mut c, "select gen_random_bytes($1::int)", &[&n]);
        assert_eq!(b.len(), n as usize);
    }
    let many: std::collections::HashSet<Vec<u8>> =
        (0..200).map(|_| bytes(&mut c, "select gen_random_bytes(16)", &[])).collect();
    assert_eq!(many.len(), 200, "16 random bytes repeated");
    let all: Vec<u8> = (0..64).flat_map(|_| bytes(&mut c, "select gen_random_bytes(1024)", &[])).collect();
    let mut seen = [0usize; 256];
    for x in &all {
        seen[*x as usize] += 1;
    }
    assert!(seen.iter().all(|&n| n > 128 && n < 384), "64 KiB of random bytes, each value about 256 times: {seen:?}");
    const B64: &str = "./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let in_alphabet = |s: &str| s.chars().all(|ch| B64.contains(ch));
    for (kind, prefix, rest) in [
        ("'bf'", "$2a$06$", 22),
        ("'bf', 12", "$2a$12$", 22),
        ("'md5'", "$1$", 8),
        ("'des'", "", 2),
        ("'xdes'", "_J9..", 4),
        ("'xdes', 3", "_1...", 4),
    ] {
        let mut salts = std::collections::HashSet::new();
        for _ in 0..50 {
            let s = text(&mut c, &format!("select gen_salt({kind})"), &[]);
            let tail = s.strip_prefix(prefix).unwrap_or_else(|| panic!("gen_salt({kind}) = {s}"));
            assert!(tail.len() == rest && in_alphabet(tail), "gen_salt({kind}) = {s}");
            salts.insert(s);
        }
        assert!(salts.len() > 40, "gen_salt({kind}) repeats: {salts:?}");
    }
}

/// `sql` cut short for a failure message: a decompression bomb's is 600 kB, and a CI log takes minutes over it.
fn short(sql: &str) -> String {
    sql.chars().take(200).collect()
}

fn run(db: &Db, s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    let o = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{}: {e}", short(sql))).pop().unwrap();
    o.rows
        .iter()
        .map(|r| r.iter().map(|v| if *v == Value::Null { "NULL".into() } else { v.to_text() }).collect())
        .collect()
}

fn one(db: &Db, s: &mut Session, sql: &str) -> String {
    run(db, s, sql)[0][0].clone()
}

fn fails(db: &Db, s: &mut Session, sql: &str) -> &'static str {
    s.run(db, sql, &[]).err().unwrap_or_else(|| panic!("{} succeeded", short(sql))).code
}

fn notices() -> Vec<String> {
    take_notices().into_iter().map(|n| format!("{}: {}", n.level, n.message)).collect()
}

/// bf, cast5 and des: Postgres on OpenSSL 3 refuses them (39000) unless OpenSSL's legacy provider is loaded,
/// and its pgp_sym_encrypt then writes the data unencrypted. The values are what Postgres 16.14's pgcrypto
/// gives with the provider loaded (OPENSSL_CONF activating `legacy` and `default`).
#[test]
fn legacy_ciphers() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    for (data, key, kind, want) in [
        ("'hello world'", "'key'", "'bf'", "6ceced43fb2a72c99822d098fcde66d8"),
        ("'hello world'", "'key'", "'bf-ecb'", "6ceced43fb2a72c90d0304c7c64f4e86"),
        ("'12345678'", "'key'", "'bf-ecb/pad:none'", "72ac01ec2b3fc486"),
        ("'hello world'", "'0123456789abcdef'", "'bf'", "954ddb4b65c05c90b18e5bb87f41f469"),
        ("'hello world'", "decode(repeat('ab', 56), 'hex')", "'bf'", "ded7c7f6244e34b933ba50b7550e4420"),
        ("'hello world'", "decode(repeat('ab', 57), 'hex')", "'bf'", "ded7c7f6244e34b933ba50b7550e4420"),
        ("'hello world'", "'k'", "'bf'", "aaa30e74bdf50008868bcb4090ae9de0"),
        ("'hello world'", "''", "'bf'", "8a3e82710c759c583255090111d0df95"),
        ("'hello world'", "'key'", "'bf-cfb'", "8cc4c0b1cd3e26d9920aa2"),
        ("'hello world'", "'key'", "'blowfish-ecb'", "6ceced43fb2a72c90d0304c7c64f4e86"),
        ("'hello world'", "'key'", "'cast5'", "a88e25b995d7a26531b85f957827d244"),
        ("'hello world'", "'0123456789abcdef'", "'cast5'", "0fd21c36d780dc8d33d2e2c907db09a9"),
        ("'hello world'", "'01234'", "'cast5'", "6664ebf67c5944f9b62b982a6d754820"),
        ("'hello world'", "'0123'", "'cast5'", "229995b8bf6db46a25f5540985530858"),
        ("'hello world'", "'0123456789abcdefg'", "'cast5'", "0fd21c36d780dc8d33d2e2c907db09a9"),
        ("'hello world'", "'key'", "'cast5-ecb'", "a88e25b995d7a2656f69f33661a51bd1"),
        ("'hello world'", "'key'", "'des'", "642df0136d226c01da8b4ccbd37357b8"),
        ("'hello world'", "'01234567'", "'des'", "9a02b43b244034e9c9c44c1d26b778af"),
        ("'hello world'", "'012345678'", "'des'", "9a02b43b244034e9c9c44c1d26b778af"),
        ("'hello world'", "'01234567'", "'des-ecb'", "9a02b43b244034e9bd2993a0bd408ea6"),
    ] {
        let sql = format!("select encode(encrypt({data}, {key}, {kind}), 'hex')");
        assert_eq!(one(&db, s, &sql), want, "{sql}");
        let back =
            format!("select convert_from(decrypt('\\x{want}', {key}, {kind}), 'UTF8') = convert_from({data}, 'UTF8')");
        assert_eq!(one(&db, s, &back), "true", "{back}");
    }
    assert_eq!(
        one(&db, s, "select encode(encrypt_iv('hello world', 'key', '12345678', 'bf'), 'hex')"),
        "6a506e0015a9d874559672926fc8b7fb"
    );
    // OpenPGP messages pgcrypto made with them: bf; cast5 and zip; bf, no MDC, a separate session key
    for (m, opts) in [
        (
            "c30d040403025de7be60366460c26cd23401664c66350dbc62d6526bc3d69894c670f3cbefc0eb82e63d3e672007dd20accb0a4be3a8b43abf726ebb9c5dc66e969a06e2df",
            "cipher-algo=bf",
        ),
        (
            "c30d040303029809895fd78645d66fd239013656053f4ad5214f960b60840a2207a114f2999c3b2274c7efdec0d6edde5401048736b2cdf05ab348d1f2848ec2f140f2e8a4cce9a6d096",
            "cipher-algo=cast5, compress-algo=1",
        ),
        (
            "c31e0404030246d24b4486945f9e7da19419c2b459a965aa25c15df375fe37b1c91df6f4813dd832e6496d5787a41980977e29bad142fa0d9d9d9140deb9a3",
            "cipher-algo=bf, disable-mdc=1, sess-key=1",
        ),
    ] {
        assert_eq!(one(&db, s, &format!("select pgp_sym_decrypt('\\x{m}', 'pw')")), "hello world", "{opts}");
        assert_eq!(fails(&db, s, &format!("select pgp_sym_decrypt('\\x{m}', 'px')")), "39000", "{opts}");
        // and what Chronos makes with them, it reads
        let sql = format!("select pgp_sym_decrypt(pgp_sym_encrypt('hello world', 'pw', '{opts}'), 'pw')");
        assert_eq!(one(&db, s, &sql), "hello world", "{opts}");
    }
    // the data is encrypted (pgcrypto on OpenSSL 3 without the legacy provider writes it in the clear)
    for algo in ["bf", "cast5"] {
        let sql =
            format!("select position('\\x68656c6c6f'::bytea in pgp_sym_encrypt('hello', 'pw', 'cipher-algo={algo}'))");
        assert_eq!(one(&db, s, &sql), "0", "{algo}");
    }
}

/// Messages gpg 2.5 (libgcrypt 1.12) made with `--symmetric`: old-format packet headers, S2K with SHA-256,
/// zip and zlib with indeterminate lengths, a SED packet (`--rfc2440`), and ciphers Postgres on OpenSSL 3 refuses.
#[test]
fn gpg_messages() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    for (m, what) in [
        (
            "8c0d040403089938b49b1e3b8eaafbd23401950b8564ffd2f6d16421f89df24ff3f939ef5b3720b2becf42dd3792283bef8006d35eace2bc4488f1a3c1b3be0f4015f2bb47",
            "blowfish",
        ),
        (
            "8c0d040303083a4ce25300344255fbd23e0100fae6ae22384f9557a4a614d44e7d4f201ebfc38c2bae6d25be2cd9cea1f3342f860afe8ad5a07f0c967703ee9ba6b5716a0d932b1fd5776e1a068432",
            "cast5, zlib",
        ),
        (
            "8c0d04020308e1c73a5f630e3186fbd23401eb9ac4b18da70ca0cc8fff15f9297fe0765abea162e31c6288a9db32c34b25d48aad47975b175508ae3739a3da1eaa6f43afce",
            "3des, sha256",
        ),
        (
            "8c0d040903082b4695413b0c6c37fbd23c0156e7438a1695ee3d77f54d8e71d0d98bd03749891ba83580468a8687a41f3a7ba7aa0841be7730bcc8d46d5be5ca00a2fb20c45e12695997d123c7",
            "aes256",
        ),
        (
            "8c0d04070308835eaad47b0c3f5cfbd240015c6bfa92687e89609621596a6ff000bbf17afe88a9fc3ff5c0f38e9abad96312a733a83a94b345a2214c97c263302fd56a8d94538ca2534163213eeea0fcdd",
            "aes, zip",
        ),
        (
            "8c0d04030302402a575bff5f52e2fbc9273fb1edfb3cfca53d3def1847a94cbfc443723ffdf89c5d0492dd674aeb5d08d481d3e13e3d5524",
            "cast5, zlib, no MDC",
        ),
    ] {
        let sql = format!("select convert_from(pgp_sym_decrypt_bytea('\\x{m}', 'pw'), 'UTF8')");
        assert_eq!(one(&db, s, &sql), "hello world", "{what}");
        // gpg marks stdin binary
        assert_eq!(fails(&db, s, &format!("select pgp_sym_decrypt('\\x{m}', 'pw')")), "39000", "{what}");
        assert_eq!(one(&db, s, &format!("select pgp_key_id('\\x{m}')")), "SYMKEY", "{what}");
    }
}

/// CREATE EXTENSION pgcrypto is kept in the world, for pg_extension; its functions are built in, before it too.
#[test]
fn create_and_drop_extension() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    let listed =
        "select extname, extversion, extnamespace::regnamespace::text from pg_extension where extname = 'pgcrypto'";
    assert!(run(&db, s, listed).is_empty());
    assert_eq!(one(&db, s, "select encode(digest('abc', 'sha1'), 'hex')"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    notices();
    assert_eq!(fails(&db, s, "drop extension pgcrypto"), "42704");
    run(&db, s, "drop extension if exists pgcrypto");
    assert_eq!(notices(), ["NOTICE: extension \"pgcrypto\" does not exist, skipping"]);
    assert_eq!(fails(&db, s, "create extension pgcrypto schema nosuch"), "3F000");
    assert_eq!(fails(&db, s, "create extension pgcrypto version '1.2'"), "22023");
    run(&db, s, "create extension pgcrypto version '1.3'");
    assert!(notices().is_empty());
    assert_eq!(run(&db, s, listed), [["pgcrypto", "1.3", "public"]]);
    assert_eq!(fails(&db, s, "create extension pgcrypto"), "42710");
    run(&db, s, "create extension if not exists pgcrypto");
    assert_eq!(notices(), ["NOTICE: extension \"pgcrypto\" already exists, skipping"]);
    run(&db, s, "drop extension pgcrypto cascade");
    assert!(run(&db, s, listed).is_empty());
    // in a schema of its own; its functions are built in, so they are found by any schema's name, as pg_dump writes
    // them (public's, ext's), whatever the record says
    run(&db, s, "create schema ext");
    run(&db, s, "create extension pgcrypto with schema ext");
    assert_eq!(run(&db, s, listed), [["pgcrypto", "1.3", "ext"]]);
    assert_eq!(one(&db, s, "select ext.crypt('pw', '$1$abc')"), "$1$abc$Kb85XxsXB.VXinPhbS4431");
    assert_eq!(one(&db, s, "select public.crypt('pw', '$1$abc')"), "$1$abc$Kb85XxsXB.VXinPhbS4431");
    // DROP SCHEMA: refused while the extension is in it, as Postgres does, and with CASCADE it goes too
    let e = s.run(&db, "drop schema ext", &[]).expect_err("drop schema ext");
    assert_eq!((e.code, e.message.as_str()), ("2BP01", "cannot drop schema ext because other objects depend on it"));
    let info = e.info.expect("a DETAIL and a HINT");
    assert_eq!(info.detail.as_deref(), Some("extension pgcrypto depends on schema ext"));
    assert_eq!(info.hint.as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    notices();
    run(&db, s, "drop schema ext cascade");
    assert_eq!(notices(), ["NOTICE: drop cascades to extension pgcrypto"]);
    assert!(run(&db, s, listed).is_empty());
    // ALTER EXTENSION: UPDATE says it's there, SET SCHEMA moves it (the functions stay where they are, built in)
    assert_eq!(fails(&db, s, "alter extension pgcrypto update"), "42704");
    run(&db, s, "create extension pgcrypto");
    run(&db, s, "alter extension pgcrypto update to '1.3'");
    assert_eq!(notices(), ["NOTICE: version \"1.3\" of extension \"pgcrypto\" is already installed"]);
    assert_eq!(fails(&db, s, "alter extension pgcrypto update to '1.4'"), "22023");
    run(&db, s, "create schema other");
    run(&db, s, "alter extension pgcrypto set schema other");
    assert_eq!(run(&db, s, listed), [["pgcrypto", "1.3", "other"]]);
    run(&db, s, "alter extension pgcrypto set schema other");
    assert_eq!(fails(&db, s, "alter extension pgcrypto set schema nosuch"), "3F000");
    assert_eq!(fails(&db, s, "alter extension pgcrypto add function digest(text, text)"), "0A000");
    assert_eq!(chronos::sql::parse("alter extension pgcrypto update").unwrap()[0].access(), chronos::sql::Reach::Write);
    run(&db, s, "drop extension pgcrypto");
    // what calls its functions by a schema's name keeps working when the extension or that schema goes (Postgres
    // refuses the first while they use it, and drops them with the second)
    run(&db, s, "create schema ext");
    run(&db, s, "create extension pgcrypto schema ext");
    run(&db, s, "create view uses_it as select ext.digest('a', 'md5') d, public.crypt('pw', 'ab') c");
    run(
        &db,
        s,
        "create function calls_it(p text) returns text language plpgsql as $$ begin return public.crypt(p, '$1$x'); end $$",
    );
    run(&db, s, "create table defaults_to_it (id int primary key, h bytea default ext.digest('x', 'sha1'))");
    run(&db, s, "drop extension pgcrypto");
    run(&db, s, "insert into defaults_to_it (id) values (1)");
    let before = (run(&db, s, "select * from uses_it"), one(&db, s, "select calls_it('pw')"));
    run(&db, s, "drop schema ext cascade");
    assert_eq!((run(&db, s, "select * from uses_it"), one(&db, s, "select calls_it('pw')")), before);
    assert_eq!(one(&db, s, "select count(h) from defaults_to_it"), "1");
    run(&db, s, "create extension pgcrypto");
    assert_eq!(one(&db, s, "select encode(public.digest('abc', 'md5'), 'hex')"), "900150983cd24fb0d6963f7d28e17f72");
    // a column default of one is worked out for each row, as in Postgres
    run(&db, s, "create table tokens (id int primary key, t bytea default gen_random_bytes(16))");
    run(&db, s, "insert into tokens (id) values (1), (2), (3)");
    assert_eq!(one(&db, s, "select count(distinct t) from tokens where length(t) = 16"), "3");
    // it changes the world: the safe mode's readers can't
    assert_eq!(
        chronos::sql::parse("create extension if not exists pgcrypto").unwrap()[0].access(),
        chronos::sql::Reach::Write
    );
    assert_eq!(
        chronos::sql::parse("drop extension if exists pgcrypto").unwrap()[0].access(),
        chronos::sql::Reach::Write
    );

    // a world forks with it, makes it, merges it, and DIFF shows it
    run(&db, s, "drop extension pgcrypto");
    run(&db, s, "create world w");
    run(&db, s, "switch world w");
    run(&db, s, "create extension pgcrypto");
    let diff = run(&db, s, "diff");
    assert!(diff.iter().any(|r| r[0] == "pgcrypto" && r[2] == "create extension"), "{diff:?}");
    assert_eq!(run(&db, s, "diff as sql"), [["create extension pgcrypto with schema public;"]]);
    // in a schema the world made too: the schema first, so the script runs
    run(&db, s, "drop extension pgcrypto");
    run(&db, s, "create schema s5");
    run(&db, s, "create extension pgcrypto schema s5");
    let script = run(&db, s, "diff as sql");
    assert_eq!(script, [["create schema s5;"], ["create extension pgcrypto with schema s5;"]]);
    run(&db, s, "switch world main");
    run(&db, s, "create world replay");
    run(&db, s, "switch world replay");
    for q in &script {
        run(&db, s, &q[0]);
    }
    assert_eq!(run(&db, s, listed), [["pgcrypto", "1.3", "s5"]]);
    run(&db, s, "switch world main");
    run(&db, s, "drop world replay");
    run(&db, s, "switch world w");
    run(&db, s, "drop extension pgcrypto");
    run(&db, s, "drop schema s5");
    run(&db, s, "create extension pgcrypto");
    run(&db, s, "switch world main");
    assert!(run(&db, s, listed).is_empty());
    run(&db, s, "create world v");
    run(&db, s, "switch world v");
    run(&db, s, "create extension pgcrypto");
    run(&db, s, "switch world main");
    run(&db, s, "merge world w");
    run(&db, s, "merge world v"); // made on both sides: one
    assert_eq!(run(&db, s, listed), [["pgcrypto", "1.3", "public"]]);
}

/// Errors whose words are pgcrypto's own, as Postgres gives them: SQLSTATE and message.
#[test]
fn error_messages_match_postgres() {
    let Some(mut pg) = oracle() else { return };
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    for sql in [
        // an S2K digest pgcrypto doesn't know (SHA-224, and 4, which is none) reads as a wrong key
        r"select pgp_sym_decrypt('\xc3040408000bd23c01b4a0d55461974901b4b8c7ce650bbf112c790b69b7e197566fa8579e331a7663d94675a1c2b5906827059ae48c21117f71a0e9580cf67dba2b88b3', 'pw')",
        r"select pgp_sym_decrypt('\xc30404080004d23c01b4a0d55461974901b4b8c7ce650bbf112c790b69b7e197566fa8579e331a7663d94675a1c2b5906827059ae48c21117f71a0e9580cf67dba2b88b3', 'pw')",
        r"select pgp_sym_decrypt('\xc30404630002d23c01b4a0d55461974901b4b8c7ce650bbf112c790b69b7e197566fa8579e331a7663d94675a1c2b5906827059ae48c21117f71a0e9580cf67dba2b88b3', 'pw')",
        r"select pgp_sym_decrypt('\xc30404080302d23c01b4a0d55461974901b4b8c7ce650bbf112c790b69b7e197566fa8579e331a7663d94675a1c2b5906827059ae48c21117f71a0e9580cf67dba2b88b3', 'pw')",
        // the _iv forms say which they are
        r"select encrypt_iv('hello', 'key', 'iv', 'aes-ecb/pad:none')",
        r"select decrypt_iv('\x00', 'key', 'iv', 'aes')",
        r"select encrypt('hello', 'key', 'aes-ecb/pad:none')",
        r"select decrypt('\x00', 'key', 'aes')",
        // text that isn't UTF-8 (gpg --textmode of a, e0 6d 04, b): the whole bad sequence
        r"select pgp_sym_decrypt('\x8c0d0407030823552e1d7bb1376bfbd23601aad8d0425f0b4c9780bfe33cb4e689fbf712835564ecd65b649433de283b48c9b17f013107fd4aa5bec07583246aef1d1ccf2d14c6', 'pw')",
        // a public-key session key packet of a version pgcrypto doesn't read
        r"select pgp_key_id('\xc10a0200000000000000000000')",
        r"select digest('abc', 'FOO')",
        r"select gen_salt('bf', 3)",
        r"select gen_random_bytes(0)",
        r"select crypt('pw', '$2a$03$abcdefghijklmnopqrstuu')",
        r"select pgp_sym_encrypt('hello', 'pw', 'compress-algo=3')",
        r"select dearmor('foo')",
    ] {
        let mine = s.run(&db, sql, &[]).err().map(|e| (e.code.to_string(), e.message));
        let theirs = pg.simple_query(sql).err().map(|e| {
            let d = e.as_db_error().expect("an error from Postgres");
            (d.code().code().to_string(), d.message().to_string())
        });
        assert_eq!(mine, theirs, "{sql}");
    }
}

/// A message a little compressed data makes much of: inflating it stops at statement_timeout (57014), as
/// everything else long does. Made here: a literal packet of `zeros` zero bytes, zip-compressed, in a SEIPD
/// packet (AES-128 from the password's MD5, a simple S2K).
fn bomb(zeros: usize) -> Vec<u8> {
    use aes::cipher::{BlockCipherEncrypt, KeyInit};
    use md5::Digest;
    let packet = |tag: u8, body: &[u8]| {
        let mut p = vec![0xc0 | tag, 0xff];
        p.extend((body.len() as u32).to_be_bytes());
        p.extend(body);
        p
    };
    // the literal data in partial chunks of 1 MiB (pgcrypto reads no length past 16 MiB, and no indeterminate one
    // in compressed data)
    let mut body = vec![b'b', 0, 0, 0, 0, 0];
    body.resize(body.len() + zeros, 0);
    let mut lit = vec![0xcb];
    let mut rest = &body[..];
    while rest.len() > 1 << 20 {
        lit.push(0xe0 | 20);
        lit.extend(&rest[..1 << 20]);
        rest = &rest[1 << 20..];
    }
    lit.push(0xff);
    lit.extend((rest.len() as u32).to_be_bytes());
    lit.extend(rest);
    drop(body);
    let mut zip = vec![1];
    zip.extend(miniz_oxide::deflate::compress_to_vec(&lit, 1));
    drop(lit);
    let mut plain = vec![0u8; 18];
    plain.extend(packet(8, &zip));
    plain.extend([0xd3, 0x14]);
    let mdc = sha1::Sha1::digest(&plain);
    plain.extend(mdc);
    let key = md5::Md5::digest(b"pw");
    let aes = aes::Aes128::new_from_slice(&key).unwrap();
    let (mut iv, mut data) = ([0u8; 16], vec![1]);
    for chunk in plain.chunks(16) {
        let mut ks = iv.into();
        aes.encrypt_block(&mut ks);
        let c: Vec<u8> = chunk.iter().zip(ks.iter()).map(|(p, k)| p ^ k).collect();
        iv[..c.len()].copy_from_slice(&c);
        data.extend(c);
    }
    let mut msg = vec![0xc3, 0x04, 0x04, 0x07, 0x00, 0x01];
    msg.extend(packet(18, &data));
    msg
}

#[test]
fn large_inflate_stops_at_statement_timeout() {
    let n = 64 << 20;
    let msg = bomb(n);
    let sql = format!(
        "select length(pgp_sym_decrypt_bytea('\\x{}', 'pw'))",
        msg.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    // it is a message Postgres reads, and so does Chronos, given the time
    if let Some(mut pg) = oracle() {
        assert_eq!(pg.query_one(sql.as_str(), &[]).unwrap().get::<_, i32>(0), n as i32);
    }
    assert_eq!(one(&db, s, &sql), n.to_string());
    // 1 ms: optimized, all 64 MiB inflate in well under 100 ms; the inflate looks at the clock every 64 KiB
    run(&db, s, "set statement_timeout = '1ms'");
    let at = std::time::Instant::now();
    assert_eq!(fails(&db, s, &sql), "57014");
    assert!(at.elapsed() < std::time::Duration::from_secs(3), "stopped after {:?}", at.elapsed());
}
