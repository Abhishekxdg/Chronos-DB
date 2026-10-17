//! Network address types: `inet`, `cidr` and `macaddr` (and `macaddr8`) as columns, literals and casts, their
//! operators (`<<`, `>>=`, `+`, `-`, `&`, `|`, `~`), functions (`host`, `netmask`, `broadcast`, `set_masklen`, ...)
//! and ordering. Postgres's answers are compared in tests/pgdiff.rs (`network_types`); these run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

/// What a query gives back, a row a string, its columns joined with `|` and booleans as `t`/`f`
/// the way psql prints them.
fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    let text = |v: &Value| match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        v => v.to_text(),
    };
    out.last().unwrap().rows.iter().map(|r| r.iter().map(text).collect::<Vec<_>>().join("|")).collect()
}

/// Statements run in order in one session: a query's rows, or the SQLSTATE of the statement
/// Postgres refuses.
fn script(steps: &[(&str, Result<&[&str], &str>)]) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, want) in steps {
        match want {
            Ok(rows_want) => assert_eq!(&rows(&mut s, &db, q), rows_want, "{q}"),
            Err(code) => {
                let e = s.run(&db, q, &[]).expect_err(q);
                assert_eq!(&e.code, code, "{q}: {}", e.message);
            }
        }
    }
}

#[test]
fn network_types() {
    script(&[
        (r"create table h (id int primary key, ip inet, net cidr, mac macaddr)", Ok(&[])),
        (
            r"insert into h values (1,'192.168.1.5/24','192.168.1.0/24','08:00:2b:01:02:03'),(2,'10.0.0.1','10.0.0.0/8','08-00-2b-01-02-04'),(3,'::1','::1/128','0800.2b01.0205')",
            Ok(&[]),
        ),
        (
            r"select * from h order by id",
            Ok(&[
                r"1|192.168.1.5/24|192.168.1.0/24|08:00:2b:01:02:03",
                r"2|10.0.0.1|10.0.0.0/8|08:00:2b:01:02:04",
                r"3|::1|::1/128|08:00:2b:01:02:05",
            ]),
        ),
        (
            r"select ip, host(ip), text(ip), family(ip), masklen(ip), netmask(ip), broadcast(ip), network(ip), abbrev(ip) from h where id<3 order by id",
            Ok(&[
                r"192.168.1.5/24|192.168.1.5|192.168.1.5/24|4|24|255.255.255.0|192.168.1.255/24|192.168.1.0/24|192.168.1.5/24",
                r"10.0.0.1|10.0.0.1|10.0.0.1/32|4|32|255.255.255.255|10.0.0.1|10.0.0.1/32|10.0.0.1",
            ]),
        ),
        (
            r"select ip << '192.168.0.0/16', ip <<= net, ip >> '192.168.1.5', net >>= ip from h where id=1",
            Ok(&[r"t|t|t|t"]),
        ),
        (
            r"select '192.168.1.5'::inet + 10, '192.168.1.5'::inet - 5, '192.168.1.9'::inet - '192.168.1.5'::inet",
            Ok(&[r"192.168.1.15|192.168.1.0|4"]),
        ),
        (
            r"select '192.168.1.5'::inet < '192.168.1.6', '10.0.0.1'::inet = '10.0.0.1/32', '1.2.3.4'::inet <> '1.2.3.5'",
            Ok(&[r"t|t|t"]),
        ),
        (
            r"select '192.168.1.5/24'::inet & '0.0.0.255', '192.168.1.5'::inet | '0.0.0.255', ~'192.168.1.5'::inet",
            Ok(&[r"0.0.0.5|192.168.1.255|63.87.254.250"]),
        ),
        (
            r"select inet_same_family('1.1.1.1','::1'), inet_merge('192.168.1.5/24','192.168.2.5/24')",
            Ok(&[r"f|192.168.0.0/22"]),
        ),
        (
            r"select set_masklen('192.168.1.5/24'::inet, 16), set_masklen('192.168.1.0/24'::cidr, 16)",
            Ok(&[r"192.168.1.5/16|192.168.0.0/16"]),
        ),
        (r"select '192.168.1.5/24'::cidr", Err(r"22P02")),
        (r"select '10.1.2.3'::cidr", Ok(&[r"10.1.2.3/32"])),
        (
            r"select macaddr '08:00:2b:01:02:03', trunc(macaddr '08:00:2b:01:02:03')",
            Ok(&[r"08:00:2b:01:02:03|08:00:2b:00:00:00"]),
        ),
        (
            r"select macaddr8 '08:00:2b:01:02:03:04:05', macaddr8_set7bit(macaddr8 '00:00:2b:01:02:03:04:05')",
            Ok(&[r"08:00:2b:01:02:03:04:05|02:00:2b:01:02:03:04:05"]),
        ),
        (r"select 'abc'::inet", Err(r"22P02")),
        (r"select '1.2.3.4/33'::inet", Err(r"22P02")),
        (r"select '192.168.1.5/24'::cidr", Err(r"22P02")),
        (r"select pg_typeof(ip), pg_typeof(net), pg_typeof(mac) from h limit 1", Ok(&[r"inet|cidr|macaddr"])),
        (r"select ip from h order by ip", Ok(&[r"10.0.0.1", r"192.168.1.5/24", r"::1"])),
        (r"select count(*), min(ip), max(ip) from h", Ok(&[r"3|10.0.0.1|::1"])),
        (
            r"select ip::text, ip::cidr, net::inet from h where id=1",
            Ok(&[r"192.168.1.5/24|192.168.1.0/24|192.168.1.0/24"]),
        ),
        (r"select inet '1.2.3.4' || 'x'", Ok(&[r"1.2.3.4/32x"])),
        (r"create index h_ip on h using gist (ip inet_ops)", Ok(&[])),
        (r"create index h_ip2 on h (ip)", Ok(&[])),
        (r"select id from h where ip = '10.0.0.1'", Ok(&[r"2"])),
        (r"select id from h where ip << '10.0.0.0/8'", Ok(&[r"2"])),
        (r"select cast('1.2.3.4' as inet), '1.2.3.4'::inet::text", Ok(&[r"1.2.3.4|1.2.3.4/32"])),
        (r"select host(ip) from h order by ip desc", Ok(&[r"::1", r"192.168.1.5", r"10.0.0.1"])),
        (r"select array['1.1.1.1','2.2.2.2']::inet[]", Ok(&[r"{1.1.1.1,2.2.2.2}"])),
        (r"select ip, sum(1) over () from h", Ok(&[r"192.168.1.5/24|3", r"10.0.0.1|3", r"::1|3"])),
        (r"select family(net), masklen(net) from h order by id", Ok(&[r"4|24", r"4|8", r"6|128"])),
        (
            r"select '::ffff:1.2.3.4'::inet, '2001:db8::1/64'::inet, host('2001:db8::1/64'::inet), network('2001:db8::1/64'::inet)",
            Ok(&[r"::ffff:1.2.3.4|2001:db8::1/64|2001:db8::1|2001:db8::/64"]),
        ),
        (r"select jsonb_build_object('ip', ip) from h where id=1", Ok(&[r#"{"ip": "192.168.1.5/24"}"#])),
        (
            r"select to_jsonb(h) from h where id=2",
            Ok(&[r#"{"id": 2, "ip": "10.0.0.1", "mac": "08:00:2b:01:02:04", "net": "10.0.0.0/8"}"#]),
        ),
        (r"select ip from h where ip in ('10.0.0.1','::1') order by id", Ok(&[r"10.0.0.1", r"::1"])),
        (r"select ip::inet from h where id = 1 group by ip", Ok(&[r"192.168.1.5/24"])),
        (r"select distinct ip from h order by ip", Ok(&[r"10.0.0.1", r"192.168.1.5/24", r"::1"])),
    ]);
}
