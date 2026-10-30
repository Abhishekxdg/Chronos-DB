//! Attach against the Postgres a customer runs: column types, key shapes, schema features and server settings.
//! CHRONOS_TEST_PG is a URL to a server where tests may create databases (skipped without it).
//!
//! Each case makes its tables twice, in `public` (attached) and in `twin` (not), runs the same changes in a world
//! and merges them, and runs them directly on the twin. Postgres's own text of both must then be equal: what a merge
//! writes is what Postgres would have done with those statements. Before and after the merge, main's reads must
//! give Postgres's text too. A case can instead expect attach to refuse it, or the merge to be refused with
//! nothing written.
//!
//! The report (one line per case and pass) goes to stderr and, as JSON lines, to CHRONOS_COMPAT_OUT when set; the
//! test fails when a case does.

use chronos::sql::run;
use chronos::{Db, MAIN};
use std::process::Command;

#[derive(Clone, Copy, PartialEq)]
enum Expect {
    /// the merge is written, and matches the twin
    Merge,
    /// the change in the world or its merge is refused, and Postgres is unchanged
    Refused,
    /// attach refuses the tables, saying this
    AttachRefused(&'static str),
}

#[derive(Clone)]
struct Case {
    name: String,
    /// lowest server_version_num it runs on
    min: u32,
    /// run once in `public` before the tables (types, extensions)
    prelude: String,
    /// makes the tables and rows, in the schema on the search_path
    tables: String,
    /// the tables attached and compared, in the case's schema
    attach: Vec<String>,
    schema: String,
    /// run in Postgres after attach, before the world
    after_attach: String,
    /// the world's changes, run on the twin too
    ops: String,
    /// a query main and Postgres must answer alike, before and after the merge
    read: Option<String>,
    /// what is compared between public and twin, per table; `{t}` is the table
    snap: String,
    expect: Expect,
    /// attach as a role with only the grants the gateway prints, not the test's own
    min_role: bool,
    /// `chronos refresh` after `after_attach`
    refresh: bool,
}

impl Case {
    fn new(name: &str, tables: &str, ops: &str) -> Case {
        Case {
            name: name.into(),
            min: 0,
            prelude: String::new(),
            tables: tables.into(),
            attach: vec!["t".into()],
            schema: "public".into(),
            after_attach: String::new(),
            ops: ops.into(),
            read: None,
            snap: "select coalesce(string_agg(x::text, E'\\n' order by x::text), '') from {t} x".into(),
            expect: Expect::Merge,
            min_role: false,
            refresh: false,
        }
    }
    fn min(mut self, v: u32) -> Case {
        self.min = v;
        self
    }
    fn prelude(mut self, s: &str) -> Case {
        self.prelude = s.into();
        self
    }
    fn attach(mut self, t: &[&str]) -> Case {
        self.attach = t.iter().map(|s| s.to_string()).collect();
        self
    }
    fn schema(mut self, s: &str) -> Case {
        self.schema = s.into();
        self
    }
    fn after_attach(mut self, s: &str) -> Case {
        self.after_attach = s.into();
        self
    }
    fn read(mut self, s: &str) -> Case {
        self.read = Some(s.into());
        self
    }
    fn snap(mut self, s: &str) -> Case {
        self.snap = s.into();
        self
    }
    fn expect(mut self, e: Expect) -> Case {
        self.expect = e;
        self
    }
    fn refresh(mut self) -> Case {
        self.refresh = true;
        self.name += ", after a refresh";
        self
    }
    fn min_role(mut self) -> Case {
        self.min_role = true;
        self
    }
}

/// A column of `ty` holding `values` (SQL literals) and a NULL: one changed to `new`, all copied by the engine to new
/// rows, one deleted, one inserted from a literal.
fn ty(name: &str, ty: &str, values: &[&str], new: &str) -> Case {
    let mut rows: Vec<String> = values.iter().enumerate().map(|(i, v)| format!("({}, {v})", i + 1)).collect();
    rows.push(format!("({}, null)", values.len() + 1));
    Case::new(
        &format!("type {name}"),
        &format!("create table t (id int primary key, v {ty}); insert into t values {};", rows.join(", ")),
        &format!(
            "update t set v = {new} where id = 1; insert into t select id + 100, v from t; \
             delete from t where id = 2; insert into t values (500, {new});"
        ),
    )
    .read("select id, coalesce(v::text, '<null>') from t order by id")
}

/// A table keyed by `ty`: one row's value changed, one row's key changed, one deleted, one inserted.
fn key(name: &str, ty: &str, keys: [&str; 5]) -> Case {
    let [a, b, c, d, e] = keys;
    Case::new(
        &format!("key {name}"),
        &format!(
            "create table t (k {ty} primary key, v text); insert into t values ({a}, 'a'), ({b}, 'b'), ({c}, 'c');"
        ),
        &format!(
            "update t set v = 'changed' where k = {a}; update t set k = {d} where k = {b}; \
             delete from t where k = {c}; insert into t values ({e}, 'new');"
        ),
    )
    .read("select k::text, v from t order by k::text")
}

fn cases() -> Vec<Case> {
    let mut c = vec![
        // A. column types
        ty("smallint", "smallint", &["-32768", "32767", "0"], "123"),
        ty("integer", "integer", &["-2147483648", "2147483647"], "42"),
        ty("bigint", "bigint", &["-9223372036854775808", "9223372036854775807"], "7"),
        ty("numeric(12,4)", "numeric(12, 4)", &["12345678.1234", "-0.0001", "0"], "1.5"),
        ty(
            "numeric",
            "numeric",
            &["123456789012345678901234567890.123456789", "'NaN'", "-0", "1e-30", "100.10"],
            "'2.50'",
        ),
        ty("numeric infinity", "numeric", &["'Infinity'", "'-Infinity'", "1"], "'Infinity'").min(140000),
        ty(
            "real",
            "real",
            &["'NaN'", "'Infinity'", "'-Infinity'", "'-0'", "3.4028235e38", "1.17549435e-38", "0.1"],
            "0.3",
        ),
        ty(
            "double precision",
            "double precision",
            &["'NaN'", "'Infinity'", "'-Infinity'", "'-0'", "1.7976931348623157e308", "5e-324", "0.1"],
            "0.30000000000000004",
        ),
        ty(
            "text",
            "text",
            &["E'emoji \\U0001F389 \\u00fc'", "E'quote '' \"dq\" \\\\ back'", "E'line\\nbreak\\ttab'", "''"],
            "'new ''x'''",
        ),
        ty("text latin", "text", &["'caf\u{e9} na\u{ef}ve \u{fc}ber'", "'plain'"], "'\u{e0} la carte'"),
        ty("varchar(5)", "varchar(5)", &["'abcde'", "'\u{e9}'"], "'xy'"),
        ty("char(5)", "char(5)", &["'ab'", "'abcde'"], "'z'"),
        ty("boolean", "boolean", &["true", "false"], "false"),
        ty("date", "date", &["'2026-10-30'", "'infinity'", "'-infinity'", "'0044-03-15 BC'"], "'2000-02-29'"),
        ty(
            "timestamp",
            "timestamp",
            &["'2026-10-30 12:34:56.789012'", "'infinity'", "'1999-12-31 23:59:59'"],
            "'2001-01-01 00:00:00.5'",
        ),
        ty(
            "timestamptz",
            "timestamptz",
            &["'2026-10-30 12:34:56.789012+05:30'", "'infinity'", "'1970-01-01 00:00:00+00'"],
            "'2001-01-01 00:00:00-08'",
        ),
        ty("time", "time", &["'23:59:59.999999'", "'00:00'"], "'12:30'"),
        ty("timetz", "timetz", &["'12:00:00+05:30'", "'23:59:59-08'"], "'01:02:03+00'"),
        ty("interval", "interval", &["'1 year 2 mons 3 days 04:05:06.7'", "'-1 day'", "'P1Y2M'"], "'90 minutes'"),
        ty(
            "uuid",
            "uuid",
            &["'00000000-0000-0000-0000-000000000001'", "'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'"],
            "'ffffffff-ffff-ffff-ffff-ffffffffffff'",
        ),
        ty("json", "json", &["'{\"b\":1,  \"a\":[1,2]}'", "'[]'", "'\"s\"'"], "'{\"z\": null}'"),
        ty("jsonb", "jsonb", &["'{\"b\":1,\"a\":{\"c\":[true,null]}}'", "'\"x\"'", "'1.50'"], "'{\"k\":\"v\"}'"),
        ty("bytea", "bytea", &["'\\x00ff10'", "'\\x'", "decode('aGVsbG8=', 'base64')"], "'\\xdeadbeef'"),
        ty("integer[]", "integer[]", &["'{1,2,3}'", "'{}'", "'{1,NULL}'", "'{{1,2},{3,4}}'"], "'{5}'"),
        ty("text[]", "text[]", &["'{\"a b\",\"c,d\",\"q\\\"uote\",NULL}'", "'{x}'"], "'{y,z}'"),
        ty("enum", "mood", &["'sad'", "'happy'"], "'ok'").prelude("create type mood as enum ('sad', 'ok', 'happy');"),
        ty("enum value added after attach", "mood", &["'sad'"], "'meh'")
            .prelude("create type mood as enum ('sad', 'ok', 'happy');")
            .after_attach("alter type mood add value 'meh';")
            .expect(Expect::Refused),
        ty("enum value added after attach", "mood", &["'sad'"], "'meh'")
            .prelude("create type mood as enum ('sad', 'ok', 'happy');")
            .after_attach("alter type mood add value 'meh';")
            .refresh(),
        ty("domain", "posint", &["1", "5"], "7").prelude("create domain posint as int check (value > 0);"),
        ty("domain check broken", "posint", &["1"], "-1")
            .prelude("create domain posint as int check (value > 0);")
            .expect(Expect::Refused),
        ty("composite", "addr", &["row('Main St', 123)::addr", "'(\"a,b\",)'"], "'(x,1)'")
            .prelude("create type addr as (street text, zip int);"),
        ty("int4range", "int4range", &["'[1,10)'", "'empty'", "'(,5]'"], "'[2,3]'"),
        // (with offsets: one without is read in UTC here, in the database's timezone there; a limit the guide lists)
        ty("tstzrange", "tstzrange", &["'[2026-01-01 00:00+00,2026-02-01 00:00+00)'"], "'[2026-03-01 00:00+05:30,)'"),
        ty("int4multirange", "int4multirange", &["'{[1,3),[5,7)}'", "'{}'"], "'{[9,10)}'").min(140000),
        ty("inet", "inet", &["'192.168.0.1/24'", "'::1'"], "'10.1.2.3'"),
        ty("cidr", "cidr", &["'10.0.0.0/8'", "'2001:db8::/32'"], "'192.168.0.0/16'"),
        ty("macaddr", "macaddr", &["'08:00:2b:01:02:03'"], "'aa:bb:cc:dd:ee:ff'"),
        ty("money", "money", &["'12.34'", "'-0.01'"], "'99.99'"),
        ty("bit(4)", "bit(4)", &["B'1010'"], "B'0001'"),
        ty("varbit", "varbit", &["B'101'", "B''"], "B'11'"),
        ty("xml", "xml", &["'<a b=\"1\">t</a>'"], "'<z/>'"),
        ty("tsvector", "tsvector", &["'a fat cat'"], "'sat:2 mat:4'"),
        ty("point", "point", &["'(1.5,2)'"], "'(0,0)'"),
        ty("citext", "citext", &["'Hello'", "'WORLD'"], "'MiXeD'").prelude("create extension if not exists citext;"),
        ty("hstore", "hstore", &["'a=>1, b=>NULL'"], "'k=>v'").prelude("create extension if not exists hstore;"),
        ty("vector", "vector(3)", &["'[1,2,3]'", "'[0.5,-1,1e-3]'"], "'[9,9,9]'")
            .prelude("create extension if not exists vector;"),
        ty("postgis geometry", "geometry", &["'SRID=4326;POINT(1 2)'"], "'SRID=4326;POINT(3 4)'")
            .prelude("create extension if not exists postgis;"),
        // B. keys
        key("text", "text", ["'a'", "'B'", "'\u{e9}'", "'renamed'", "'new'"]),
        key(
            "uuid",
            "uuid",
            [
                "'00000000-0000-0000-0000-000000000001'",
                "'00000000-0000-0000-0000-000000000002'",
                "'00000000-0000-0000-0000-000000000003'",
                "'00000000-0000-0000-0000-000000000004'",
                "'00000000-0000-0000-0000-000000000005'",
            ],
        ),
        key("bigint", "bigint", ["1", "9223372036854775807", "-9223372036854775808", "2", "3"]),
        key("date", "date", ["'2026-01-01'", "'2026-01-02'", "'2026-01-03'", "'2027-01-01'", "'2028-01-01'"]),
        key(
            "timestamptz",
            "timestamptz",
            [
                "'2026-01-01 00:00:00+00'",
                "'2026-01-01 00:00:00.000001+00'",
                "'2026-01-01 05:30:00+05:00'",
                "'2027-01-01 00:00:00+00'",
                "'2028-01-01 00:00:00+00'",
            ],
        ),
        key("numeric", "numeric", ["1.0", "2.50", "3", "4.000", "5"]),
        key("case only", "text collate \"C\"", ["'a'", "'A'", "'b'", "'B'", "'c'"]),
        key("nondeterministic collation", "text collate ci", ["'Abc'", "'xyz'", "'q'", "'XYZ2'", "'new'"])
            .prelude("create collation ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);"),
        Case::new(
            "key composite",
            "create table t (a int, b text, v text, primary key (a, b));
             insert into t values (1, 'x', 'one'), (1, 'y', 'two'), (2, 'x', 'three');",
            "update t set v = 'changed' where a = 1 and b = 'x'; update t set b = 'z' where a = 1 and b = 'y';
             delete from t where a = 2; insert into t values (3, 'x', 'new');",
        ),
        // C. schema features
        Case::new(
            "defaults",
            "create table t (id serial primary key, created timestamptz not null default now(),
                 u uuid default gen_random_uuid(), s text default 'x', n int not null);
             insert into t (n) values (1);",
            "insert into t (n) values (2); insert into t (n, s) values (3, 'given');",
        )
        .min(130000)
        .snap("select string_agg(format('%s %s %s %s', n, s, created is not null, u is not null), ',' order by n) from {t}"),
        Case::new(
            "not null broken",
            "create table t (id int primary key, n int not null); insert into t values (1, 1);",
            "update t set n = null where id = 1;",
        )
        .expect(Expect::Refused),
        Case::new(
            "check broken",
            "create table t (id int primary key, n int check (n > 0)); insert into t values (1, 1);",
            "update t set n = -1 where id = 1;",
        )
        .expect(Expect::Refused),
        Case::new(
            "unique broken",
            "create table t (id int primary key, e text unique); insert into t values (1, 'a'), (2, 'b');",
            "update t set e = 'a' where id = 2;",
        )
        .expect(Expect::Refused),
        Case::new(
            "unique changed only in Postgres",
            "create table t (id int primary key, e text); insert into t values (1, 'a'), (2, 'b');",
            "update t set e = 'a' where id = 2;",
        )
        .after_attach("create unique index on t (e);")
        .expect(Expect::Refused),
        Case::new(
            "foreign key between attached",
            "create table p (id int primary key); create table t (id int primary key, p int references p (id));
             insert into p values (1), (2); insert into t values (1, 1);",
            "insert into p values (3); insert into t values (2, 3); delete from t where id = 1; delete from p where id = 1;",
        )
        .attach(&["p", "t"]),
        Case::new(
            "foreign key broken",
            "create table p (id int primary key); create table t (id int primary key, p int references p (id));
             insert into p values (1);",
            "insert into t values (9, 99);",
        )
        .attach(&["p", "t"])
        .expect(Expect::Refused),
        Case::new(
            "foreign key to a table not attached",
            "create table p (id int primary key); create table t (id int primary key, p int references p (id));
             insert into p values (1), (2);",
            "insert into t values (1, 2);",
        ),
        Case::new(
            "foreign key to a table not attached broken",
            "create table p (id int primary key); create table t (id int primary key, p int references p (id));
             insert into p values (1);",
            "insert into t values (1, 99);",
        )
        .expect(Expect::Refused),
        Case::new(
            "on delete cascade, both attached",
            "create table p (id int primary key);
             create table t (id int primary key, p int references p (id) on delete cascade);
             insert into p values (1), (2); insert into t values (1, 1), (2, 1), (3, 2);",
            "delete from p where id = 1;",
        )
        .attach(&["p", "t"])
        .read("select count(*)::text from t"),
        Case::new(
            "on delete cascade into a table not attached",
            "create table t (id int primary key);
             create table c (id int primary key, t int references t (id) on delete cascade);
             insert into t values (1), (2); insert into c values (1, 1), (2, 2);",
            "delete from t where id = 1;",
        )
        .snap("select (select coalesce(string_agg(x::text, ',' order by x::text), '') from {t} x) || ' / ' || \
               (select coalesce(string_agg(y::text, ',' order by y::text), '') from c y)"),
        Case::new(
            "quoted names",
            "create table \"Order Items\" (\"Select\" int primary key, \"camelCase\" text, \"has space\" text, \"from\" int);
             insert into \"Order Items\" values (1, 'a', 'b', 1), (2, 'c', 'd', 2);",
            "update \"Order Items\" set \"camelCase\" = 'z' where \"Select\" = 1;
             insert into \"Order Items\" values (3, 'e', 'f', 3); delete from \"Order Items\" where \"from\" = 2;",
        )
        .attach(&["Order Items"]),
        Case::new(
            "schema other than public",
            "create table t (id int primary key, v text); insert into t values (1, 'a'), (2, 'b');",
            "update app.t set v = 'z' where id = 1; insert into app.t values (3, 'c'); delete from app.t where id = 2;",
        )
        .schema("app"),
        Case::new(
            "dropped column",
            "create table t (id int primary key, a text, gone text, b int); alter table t drop column gone;
             insert into t values (1, 'x', 1), (2, 'y', 2);",
            "update t set b = 9 where id = 1; insert into t values (3, 'z', 3); delete from t where id = 2;",
        )
        .read("select id, a, b from t order by id"),
        Case::new(
            "column added in Postgres after attach",
            "create table t (id int primary key, a text); insert into t values (1, 'x');",
            "insert into t values (2, 'y');",
        )
        .after_attach("alter table t add column extra int default 7;")
        .expect(Expect::Refused)
        .snap("select coalesce(string_agg(format('%s %s', id, a), ',' order by id), '') from {t}"),
        Case::new(
            "column added in Postgres after attach",
            "create table t (id int primary key, a text); insert into t values (1, 'x');",
            "insert into t values (2, 'y');",
        )
        .after_attach("alter table t add column extra int default 7;")
        .refresh()
        .snap("select coalesce(string_agg(format('%s %s', id, a), ',' order by id), '') from {t}"),
        Case::new(
            "wide table",
            &format!(
                "create table t (id int primary key, {}); insert into t (id, c1, c150) values (1, 'a', 'z');",
                (1..=150).map(|i| format!("c{i} text")).collect::<Vec<_>>().join(", ")
            ),
            "update t set c75 = 'mid' where id = 1; insert into t (id, c2, c149) values (2, 'b', 'y');",
        ),
        Case::new(
            "toasted values",
            "create table t (id int primary key, v text, b bytea);
             insert into t values (1, repeat('x', 1000000), convert_to(repeat('ab', 2500000), 'UTF8'));",
            "insert into t select id + 100, v || 'y', b from t; update t set v = repeat('q', 2000000) where id = 1;",
        )
        .snap("select string_agg(id || ':' || md5(v) || ':' || md5(b), ',' order by id) from {t}"),
        Case::new(
            "table inheritance refused",
            "create table t (id int primary key, v text); create table t_child () inherits (t);",
            "insert into t values (1, 'a');",
        )
        .expect(Expect::AttachRefused("")),
        // D33: the role the gateway's check prints
        Case::new(
            "role with only the printed grants",
            "create table t (id serial primary key, v text); insert into t (v) values ('a'), ('b');",
            "update t set v = 'z' where id = 1; insert into t (v) values ('c'); delete from t where id = 2;",
        )
        .min_role()
        .snap("select coalesce(string_agg(v, ',' order by v), '') from {t}"),
    ];
    for x in &mut c {
        if x.read.is_none() && x.expect == Expect::Merge && x.attach == ["t"] && x.schema == "public" {
            x.read = Some("select count(*)::text from t".into());
        }
    }
    c
}

/// A database's settings for a pass: `create database` options, then `alter database ... set` ones.
struct Pass {
    name: &'static str,
    create: &'static str,
    set: &'static [&'static str],
}

const PASSES: &[Pass] = &[
    Pass { name: "default", create: "", set: &[] },
    Pass {
        name: "settings",
        create: "",
        set: &[
            "timezone = 'Asia/Kathmandu'",
            "datestyle = 'SQL, DMY'",
            "intervalstyle = 'sql_standard'",
            "extra_float_digits = 0",
            "bytea_output = 'escape'",
            "search_path = app, public",
        ],
    },
    Pass { name: "latin1", create: "encoding 'LATIN1' lc_collate 'C' lc_ctype 'C' template template0", set: &[] },
    Pass { name: "sql_ascii", create: "encoding 'SQL_ASCII' lc_collate 'C' lc_ctype 'C' template template0", set: &[] },
];

fn pg_text(pg: &mut postgres::Client, sql: &str) -> Result<String, String> {
    let r = pg.simple_query(sql).map_err(|e| e.as_db_error().map_or(e.to_string(), |d| d.message().to_string()))?;
    Ok(r.iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => {
                Some((0..r.len()).map(|i| r.get(i).unwrap_or("<NULL>").to_string()).collect::<Vec<_>>().join("|"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn engine_text(db: &Db, sql: &str) -> Result<String, String> {
    let r = run(db, MAIN, sql, &[]).map_err(|e| e.message)?;
    let last = r.into_iter().last().ok_or("no result")?;
    Ok(last
        .rows
        .iter()
        .map(|row| row.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn with_db(admin_url: &str, name: &str) -> String {
    match admin_url.rsplit_once('/') {
        Some((base, rest)) => {
            let q = rest.split_once('?').map(|(_, q)| format!("?{q}")).unwrap_or_default();
            format!("{base}/{name}{q}")
        }
        None => panic!("CHRONOS_TEST_PG needs a database in its path"),
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn sorted(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    lines.join("\n")
}

/// A read in Postgres as main shows values (ISO dates, UTC, hex bytea, shortest exact floats), its rows sorted: the
/// database's own display settings and its collation's order aren't what main is compared on.
fn pg_read(pg: &mut postgres::Client, schema: &str, q: &str) -> String {
    let set = "set datestyle = ISO, MDY; set intervalstyle = postgres; set extra_float_digits = 1; \
               set bytea_output = hex; set timezone = UTC";
    let t = pg_text(pg, &format!("{set}; set search_path = {schema}, public; {q}")).unwrap();
    pg.batch_execute(
        "reset datestyle; reset intervalstyle; reset extra_float_digits; reset bytea_output; reset timezone",
    )
    .unwrap();
    sorted(&t)
}

/// The URL attach and merges use: the test's own, or CHRONOS_COMPAT_SOURCE with `{user}`, `{password}` and `{db}`
/// filled in (another way to the same server: TLS checked, through PgBouncer).
fn source_url(url: &str, db: &str, role: Option<(&str, &str)>) -> String {
    let userinfo = url.split_once("://").and_then(|(_, r)| r.split_once('@')).map(|(u, _)| u).unwrap_or("");
    let (user, password) = role.unwrap_or_else(|| userinfo.split_once(':').unwrap_or((userinfo, "")));
    match std::env::var("CHRONOS_COMPAT_SOURCE") {
        Ok(t) => t.replace("{user}", user).replace("{password}", password).replace("{db}", db),
        Err(_) => match role {
            Some(_) => format!("postgres://{user}:{password}@{}", url.split_once('@').map_or(url, |(_, r)| r)),
            None => url.to_string(),
        },
    }
}

fn first_diff(a: &str, b: &str) -> String {
    let (al, bl): (Vec<_>, Vec<_>) = (a.lines().collect(), b.lines().collect());
    for i in 0..al.len().max(bl.len()) {
        let (x, y) = (al.get(i).copied().unwrap_or("<none>"), bl.get(i).copied().unwrap_or("<none>"));
        if x != y {
            let cut = |s: &str| s.chars().take(160).collect::<String>();
            return format!("line {}: {:?} vs {:?}", i + 1, cut(x), cut(y));
        }
    }
    "equal".into()
}

/// One case in one pass: "PASS", "SKIP: why" or "FAIL: why".
fn run_case(admin_url: &str, version: u32, pass: &Pass, n: usize, case: &Case) -> String {
    if version < case.min {
        return format!("SKIP: needs server_version_num {}", case.min);
    }
    let name = format!("chronos_compat_{}_{n}_{}", pass.name, std::process::id());
    let mut admin = postgres::Client::connect(admin_url, postgres::NoTls).unwrap();
    admin.batch_execute(&format!("drop database if exists {name}")).unwrap();
    if let Err(e) = admin.batch_execute(&format!("create database {name} {}", pass.create)) {
        return format!("SKIP: create database: {e}");
    }
    let result = run_in(admin_url, &name, pass, n, case);
    let _ = admin.batch_execute(&format!(
        "select pg_terminate_backend(pid) from pg_stat_activity where datname = '{name}' and pid <> pg_backend_pid()"
    ));
    let _ = admin.batch_execute(&format!("drop database if exists {name}"));
    let _ = admin.batch_execute(&format!("drop role if exists {name}_r"));
    result
}

fn run_in(admin_url: &str, name: &str, pass: &Pass, n: usize, case: &Case) -> String {
    let url = with_db(admin_url, name);
    let mut pg = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    for s in pass.set {
        pg.batch_execute(&format!("alter database {name} set {s}")).unwrap();
    }
    drop(pg);
    let mut pg = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let s = &case.schema;
    let setup = format!(
        "create schema if not exists app; set search_path = public; {prelude}
         create schema if not exists twin; set search_path = {s}, public; {tables}
         set search_path = twin, public; {tables} reset search_path;",
        prelude = case.prelude,
        tables = case.tables,
    );
    if let Err(e) = pg.batch_execute(&setup) {
        let m = e.as_db_error().map_or(e.to_string(), |d| d.message().to_string());
        return format!("SKIP: setup in Postgres: {m}");
    }
    let snap = |pg: &mut postgres::Client, schema: &str| -> Result<String, String> {
        let mut out = vec![];
        for t in &case.attach {
            let q = case.snap.replace("{t}", &format!("{schema}.{}", quote(t)));
            out.push(pg_text(pg, &format!("set search_path = {schema}, public; {q}"))?);
        }
        Ok(out.join("\n--\n"))
    };
    let before = snap(&mut pg, s).unwrap();

    let source = if case.min_role {
        let tables: Vec<String> = case.attach.iter().map(|t| format!("{s}.{}", quote(t))).collect();
        let grants = format!(
            "drop role if exists {name}_r; create role {name}_r login password 'compat-pw';
             grant connect on database {name} to {name}_r;
             grant usage on schema {s} to {name}_r;
             grant select, insert, update, delete on {} to {name}_r;
             grant usage on all sequences in schema {s} to {name}_r;
             create schema if not exists chronos;
             create table if not exists chronos.merges (id text primary key, at timestamptz not null default now());
             grant usage on schema chronos to {name}_r;
             grant select, insert on chronos.merges to {name}_r;",
            tables.join(", ")
        );
        pg.batch_execute(&grants).unwrap();
        source_url(&url, name, Some((&format!("{name}_r"), "compat-pw")))
    } else {
        source_url(&url, name, None)
    };
    // merges in this process write to CHRONOS_SOURCE_URL; cases run one at a time
    unsafe { std::env::set_var("CHRONOS_SOURCE_URL", &source) };

    let dir = std::env::temp_dir().join(format!("chronos-compat-{}-{n}-{}", pass.name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let list: Vec<String> = case.attach.iter().map(|t| format!("{s}.{t}")).collect();
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .arg("attach")
        .arg(&dir)
        .args(["--tables", &list.join(",")])
        .env("CHRONOS_SOURCE_URL", &source)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect::<String>();
    if let Expect::AttachRefused(why) = case.expect {
        let _ = std::fs::remove_dir_all(&dir);
        return if !out.status.success() && said.contains(why) {
            format!("PASS (refused: {})", one_line(&said))
        } else if out.status.success() {
            "FAIL: attach took it".into()
        } else {
            format!("FAIL: refused without saying {why:?}: {}", one_line(&said))
        };
    }
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return format!("FAIL: attach: {}", one_line(&said));
    }
    let result = (|| -> String {
        let mut db = match Db::open(&dir) {
            Ok(db) => db,
            Err(e) => return format!("FAIL: open: {e:?}"),
        };
        let mut notes = vec![];
        if let Some(q) = &case.read {
            let theirs = pg_read(&mut pg, s, q);
            match engine_text(&db, q).map(|t| sorted(&t)) {
                Ok(ours) if ours == theirs => {}
                Ok(ours) => notes.push(format!("read after attach differs, {}", first_diff(&ours, &theirs))),
                Err(e) => notes.push(format!("read after attach: {e}")),
            }
        }
        if !case.after_attach.is_empty() {
            pg.batch_execute(&format!("set search_path = {s}, public; {} reset search_path;", case.after_attach))
                .unwrap();
        }
        if case.refresh {
            drop(db);
            let out = Command::new(env!("CARGO_BIN_EXE_chronos")).arg("refresh").arg(&dir).output().unwrap();
            if !out.status.success() {
                return format!("FAIL: refresh: {}", one_line(&String::from_utf8_lossy(&out.stderr)));
            }
            db = Db::open(&dir).unwrap();
        }
        let world = "compat";
        if let Err(e) = run(&db, MAIN, &format!("create world {world}"), &[]) {
            return format!("FAIL: create world: {}", e.message);
        }
        let ops = run(&db, world, &case.ops, &[]).err().map(|e| e.message);
        let merge = match &ops {
            Some(_) => None,
            None => run(&db, MAIN, &format!("merge world {world}"), &[]).err().map(|e| e.message),
        };
        let after = snap(&mut pg, s).unwrap();
        if case.expect == Expect::Refused {
            return match (ops, merge) {
                (None, None) => format!("FAIL: merged what Postgres refuses: {}", first_diff(&after, &before)),
                (o, m) if after == before => format!("PASS (refused: {})", one_line(&o.or(m).unwrap())),
                _ => format!("FAIL: refused but Postgres changed: {}", first_diff(&after, &before)),
            };
        }
        if let Some(e) = ops {
            return format!("FAIL: change in world: {}", one_line(&e));
        }
        if let Some(e) = merge {
            return format!("FAIL: merge: {}", one_line(&e));
        }
        let twin_ops =
            format!("set search_path = twin, public; {} reset search_path;", case.ops.replace("app.", "twin."));
        if let Err(e) = pg.batch_execute(&twin_ops) {
            return format!("FAIL: the case's changes fail in Postgres itself: {e}");
        }
        let twin = snap(&mut pg, "twin").unwrap();
        if after != twin {
            return format!("FAIL: written differs from Postgres doing it, {}", first_diff(&after, &twin));
        }
        if let Some(q) = &case.read {
            let theirs = pg_read(&mut pg, s, q);
            match engine_text(&db, q).map(|t| sorted(&t)) {
                Ok(ours) if ours == theirs => {}
                Ok(ours) => notes.push(format!("main after merge differs, {}", first_diff(&ours, &theirs))),
                Err(e) => notes.push(format!("read after merge: {e}")),
            }
        }
        if notes.is_empty() { "PASS".into() } else { format!("FAIL: written right; {}", notes.join("; ")) }
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[test]
fn attach_compat_matrix() {
    let Ok(admin_url) = std::env::var("CHRONOS_TEST_PG") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    let mut admin = postgres::Client::connect(&admin_url, postgres::NoTls).unwrap();
    let version: u32 = pg_text(&mut admin, "show server_version_num").unwrap().parse().unwrap();
    let full = pg_text(&mut admin, "show server_version").unwrap();
    drop(admin);
    let only = std::env::var("CHRONOS_COMPAT_ONLY").ok();
    let passes: Vec<&Pass> = match std::env::var("CHRONOS_COMPAT_PASSES") {
        Ok(p) => PASSES.iter().filter(|x| p.split(',').any(|n| n == x.name)).collect(),
        Err(_) => PASSES.iter().collect(),
    };
    let mut out = std::env::var("CHRONOS_COMPAT_OUT")
        .ok()
        .map(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap());
    let mut failed = vec![];
    for pass in passes {
        for (n, case) in cases().iter().enumerate() {
            if only.as_deref().is_some_and(|o| !case.name.contains(o)) {
                continue;
            }
            let r = run_case(&admin_url, version, pass, n, case);
            eprintln!("pg {full} | {} | {} | {r}", pass.name, case.name);
            if let Some(f) = out.as_mut() {
                use std::io::Write;
                let line = serde_json::json!({"pg": full, "pass": pass.name, "case": case.name, "result": r});
                writeln!(f, "{line}").unwrap();
            }
            if r.starts_with("FAIL") {
                failed.push(format!("{} / {}: {r}", pass.name, case.name));
            }
        }
    }
    assert!(failed.is_empty(), "{} failed:\n{}", failed.len(), failed.join("\n"));
}
