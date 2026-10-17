//! Text functions: `translate`, `unistr` and `U&'...'`, `sha224`, `chr`, the letter case of
//! `upper`/`lower`, the `~~` and `^@` operators, `LIKE ANY`, and `substring` with a pattern. Postgres's answers are compared in tests/pgdiff.rs (`text_functions_*`);
//! these expectations, all taken from a real Postgres 16, run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

/// What a query gives back, a row a string, its columns joined with `|` and booleans as `t`/`f`
/// the way psql prints them.
fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    let text = |v: &Value| match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Value::Bytea(b) => format!("\\x{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        v => v.to_text(),
    };
    out.last().unwrap().rows.iter().map(|r| r.iter().map(text).collect::<Vec<_>>().join("|")).collect()
}

fn all(cases: &[(&str, &[&str])]) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, want) in cases {
        assert_eq!(&rows(&mut s, &db, q), want, "{q}");
    }
}

/// Statements Postgres refuses, and the SQLSTATE it gives.
fn refused(cases: &[(&str, &str)]) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, code) in cases {
        let e = s.run(&db, q, &[]).expect_err(q);
        assert_eq!(&e.code, code, "{q}: {}", e.message);
    }
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
fn translate_unistr_sha224_and_case() {
    all(&[
        (
            r"select translate('hello','el','ip'), translate('12345','143','ax'), translate('abc','abc',''), translate('abc','','x'), translate('a','aa','xy')",
            &[r"hippo|a2x5||abc|x"],
        ),
        (
            r"select translate(null,'a','b'), translate('a',null,'b'), translate('a','b',null), translate(null,null,null)",
            &[r"|||"],
        ),
        (
            r"select translate('abc','ab','x'), translate('éa','éa','ea'), translate('héllo wörld','éö','eo'), translate('','a','b'), translate('abc','abcabc','xyzuvw')",
            &[r"xc|ea|hello world||xyz"],
        ),
        (
            r"select character_length('héllo'), char_length('héllo'), character_length(''), character_length(null)",
            &[r"5|5|0|"],
        ),
        (
            r"select encode(sha224('a'::bytea), 'hex'), encode(sha224(''::bytea), 'hex'), encode(sha224('abc'::bytea), 'hex')",
            &[
                r"abd37534c7d9a2efb9465de931cd7055ffdb8879563ae98078d6d6d5|d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f|23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7",
            ],
        ),
        (
            r"select sha224('a'), pg_typeof(sha224('a'))",
            &[r"\xabd37534c7d9a2efb9465de931cd7055ffdb8879563ae98078d6d6d5|bytea"],
        ),
        (
            r"select encode(sha224(repeat('abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu', 100)::bytea), 'hex')",
            &[r"77ef38fe67e0639f290051b876aaf2c30e7471bff105c0dd41f3249d"],
        ),
        (
            r"select encode(sha224(repeat('a', 55)::bytea), 'hex'), encode(sha224(repeat('a', 56)::bytea), 'hex'), encode(sha224(repeat('a', 64)::bytea), 'hex'), encode(sha224(repeat('a', 119)::bytea), 'hex')",
            &[
                r"fb0bd626a70c28541dfa781bb5cc4d7d7f56622a58f01a0b1ddd646f|d40854fc9caf172067136f2e29e1380b14626bf6f0dd06779f820dcd|a88cd5cde6d6fe9136a4e58b49167461ea95d388ca2bdb7afdc3cbf4|e000e6709d26667b631faa7fc1bd404eb4774003c5fb4f51a0184875",
            ],
        ),
        (r"select sha224(null::bytea) is null", &[r"t"]),
        (
            r"select unistr('\0041'), unistr('d\0061t\+000061'), unistr('\\'), unistr('a''b'), unistr(''), unistr(null)",
            &[r"A|data|\|a'b||"],
        ),
        (
            r"select unistr('\d83d\de00'), unistr('\d83d\+00de00'), unistr('\0041\0042'), unistr('A\U00000042\+000043'), length(unistr('\+01f600'))",
            &[r"😀|😀|AB|ABC|1"],
        ),
        (r"select U&'d\0061t', U&'\+000061', U&'d!0061t' UESCAPE '!', U&'a\\b', U&'x'", &[r"dat|a|dat|a\b|x"]),
        (r"select U&'\d83d\de00', U&'a!!b' uescape '!', U&'x' uescape '\'", &[r"😀|a!b|x"]),
        (r"select U&'a!0041' uescape '!', U&'\0041' as uescape", &[r"aA|A"]),
        (r"select u&'\0041'  ,  U&'it''s'", &[r"A|it's"]),
        (r"select chr(233), chr(65), chr(128512), chr(1114111) is not null, ascii(chr(128512))", &[r"é|A|😀|t|128512"]),
        (
            r"select upper('straße'), lower('İ'), upper('ǅ'), lower('ǅ'), upper('ﬁ'), upper('ŉ'), upper('ΐ'), lower('Σ'), upper('ς'), lower('ẞ'), upper('ᾳ'), upper('ı'), lower('K'), upper('ǆ')",
            &[r"STRAßE|i|Ǆ|ǆ|ﬁ|ŉ|ΐ|σ|Σ|ß|ᾼ|I|k|Ǆ"],
        ),
        (
            r"select lower('ΑΣ'), lower('ΑΣ Σ'), upper('ᾀ'), upper('ᾡ'), upper('ῃ'), initcap('ǆa ǅb'), initcap('ßa straße'), initcap('ΑΣ ΑΣΑ')",
            &[r"ασ|ασ σ|ᾈ|ᾩ|ῌ|Ǆa Ǆb|ßa Straße|Ασ Ασα"],
        ),
        (r"select 'abc'::bpchar, 'ab'::character varying(5)", &[r"abc|ab"]),
    ]);
    refused(&[
        (r"select unistr('\z')", r"42601"),
        (r"select unistr('\00')", r"42601"),
        (r"select unistr('\+0000zz')", r"42601"),
        (r"select unistr('abc\')", r"42601"),
        (r"select unistr('\d800')", r"42601"),
        (r"select unistr('\dc00')", r"42601"),
        (r"select unistr('\d83dx')", r"42601"),
        (r"select unistr('\0000')", r"22023"),
        (r"select unistr('\+110000')", r"22023"),
        (r"select unistr('\U00110000')", r"22023"),
        (r"select unistr('\d83d\0041')", r"42601"),
        (r"select U&'\zz'", r"42601"),
        (r"select U&'\dc00'", r"42601"),
        (r"select U&'\0000'", r"42601"),
        (r"select U&'\+110000'", r"42601"),
        (r"select U&'a\b'", r"42601"),
        (r"select U&'\0041' uescape '+'", r"42601"),
        (r"select U&'x' uescape 'ab'", r"42601"),
        (r"select U&'x' uescape ''", r"42601"),
        (r"select U&'x' uescape ' '", r"42601"),
        (r"select U&'x' uescape '0'", r"42601"),
        (r#"select U&'x' uescape '"'"#, r"42601"),
        (r"select chr(0)", r"54000"),
        (r"select chr(-1)", r"22023"),
        (r"select chr(1114112)", r"54000"),
        (r"select chr(55296)", r"54000"),
        (r"select chr(57343)", r"54000"),
        (r"select chr(2147483648)", r"42883"),
    ]);
}

#[test]
fn like_operators_quantifiers_and_substring_patterns() {
    script(&[
        (
            r"select 'abc' ~~ 'a%', 'abc' !~~ 'a%', 'abc' ~~* 'A%', 'abc' !~~* 'A%', 'abc' ~~ 'b%', 'abc' !~~ 'b%'",
            Ok(&[r"t|f|t|f|f|t"]),
        ),
        (
            r"select 'a%c' ~~ 'a\%c', 'abc' ~~ 'a\%c', 'a_c' ~~ 'a\_c', null ~~ 'a', 'a' ~~ null, 'a' !~~ null",
            Ok(&[r"t|f|t|||"]),
        ),
        (
            r"select 'abc' ^@ 'a', 'abc' ^@ 'b', 'abc' ^@ '', '' ^@ 'a', null ^@ 'a', 'abc' ^@ null, 'héllo' ^@ 'hé'",
            Ok(&[r"t|f|t|f|||t"]),
        ),
        (
            r"select 'abc' like any (array['a%','b%']), 'abc' like all (array['a%','%c']), 'abc' ilike any (array['A%']), 'abc' not like all (array['x%','y%'])",
            Ok(&[r"t|t|t|t"]),
        ),
        (
            r"select 'abc' like any ('{a%,b%}'), 'abc' like all ('{a%,%z}'), 'abc' not like any ('{a%,%z}'), 'abc' not like all ('{a%,%z}')",
            Ok(&[r"t|f|t|f"]),
        ),
        (
            r"select 'abc' like any (array['x%', null]), 'abc' like any (array['a%', null]), 'abc' like all (array['a%', null]), 'abc' like all (array['x%', null])",
            Ok(&[r"|t||f"]),
        ),
        (
            r"select 'abc' like any (array[]::text[]), 'abc' like all (array[]::text[]), 'abc' not like any (array[]::text[]), 'abc' not like all (array[]::text[])",
            Ok(&[r"f|t|f|t"]),
        ),
        (r"select null like any (array['a']), 'a' like any (null::text[]), 'a' like all (null::text[])", Ok(&[r"||"])),
        (
            r"select 'abc' ~~ any (array['a%','z']), 'abc' !~~ all (array['z%','y%']), 'abc' ~~* all (array['A%','%C']), 'abc' !~~* any (array['a%','A%'])",
            Ok(&[r"t|t|t|f"]),
        ),
        (r"select 'abc' some like ('x')", Err(r"42601")),
        (
            r"select 'abc' like some (array['x%','a%']), 'abc' ilike some (array['X%','A%']), 'abc' not ilike any (array['A%','B%']), 'abc' not ilike all (array['A%','B%'])",
            Ok(&[r"t|t|t|f"]),
        ),
        (r"select 'a_c' like any (array['a\_c']), 'abc' like any (array['a\_c'])", Ok(&[r"t|f"])),
        (
            r"select 'abc' ~ any (array['x','b']), 'abc' !~ all (array['x','y']), 'abc' ~* any (array['B']), 'abc' ~ all (array['a','b']), 'abc' ~ all (array['a','z']), 'abc' !~ any (array['a','z'])",
            Ok(&[r"t|t|t|t|f|t"]),
        ),
        (
            r"select 'abc' ~ any (array['x', null]), 'abc' ~ any (array['b', null]), 'abc' ~ all (array['b', null]), 'abc' ~ all (array['z', null]), 'abc' ~ any (array[]::text[]), 'abc' ~ all (array[]::text[])",
            Ok(&[r"|t||f|f|t"]),
        ),
        (r"select 'abc' ~ any ('{a,z}'), 'abc' ~* all ('{A,B}'), 'abc' !~* any ('{A,Z}')", Ok(&[r"t|t|t"])),
        (r"create table lk (id int primary key, name text, pats text[])", Ok(&[])),
        (
            r"insert into lk values (1, 'alpha', '{a%,%z}'), (2, 'beta', '{b%,x%}'), (3, 'gamma', '{z%}'), (4, null, '{a%}'), (5, 'delta', null)",
            Ok(&[]),
        ),
        (r"select id from lk where name like any (pats) order by id", Ok(&[r"1", r"2"])),
        (r"select id from lk where name like all (pats) order by id", Ok(&[])),
        (r"select id from lk where name not like all (pats) order by id", Ok(&[r"3"])),
        (r"select id from lk where name ~ any (pats) order by id", Ok(&[])),
        (r"select id from lk where name ilike any (array['A%', 'D%']) order by id", Ok(&[r"1", r"5"])),
        (
            r"select id from lk where name like any (select p from (values ('a%'), ('g%')) v(p)) order by id",
            Ok(&[r"1", r"3"]),
        ),
        (
            r"select id from lk where name like all (select p from (values ('a%'), ('%a')) v(p)) order by id",
            Ok(&[r"1"]),
        ),
        (r"select id from lk where name ~~ 'a%' or name ^@ 'g' order by id", Ok(&[r"1", r"3"])),
        (
            r"select id, name ~~ 'a%', name ^@ 'a' from lk order by id",
            Ok(&[r"1|t|t", r"2|f|f", r"3|f|f", r"4||", r"5|f|f"]),
        ),
        (
            r"select substring('abcdef' from '..$'), substring('Thomas' from '...$'), substring('Thomas' from 2 for 3)",
            Ok(&[r"ef|mas|hom"]),
        ),
        (
            r"select substring('foobar' from 'o(.)b'), substring('foobar', 'o.b'), substring('abc' from 'z'), substring('abc' from '')",
            Ok(&[r"o|oob||"]),
        ),
        (
            r#"select substring('foobar' from '%#"o_b#"%' for '#'), substring('abcdef' similar '%#"cd#"%' escape '#'), substring('abcdef' from '#"ab#"%' for '#')"#,
            Ok(&[r"oob|cd|ab"]),
        ),
        (
            r"select substring('foobar' from 'o+(b)?(x)?'), substring('foobar' from '(x)?o+'), substring('foobar' from '(?:o+)(b)')",
            Ok(&[r"b||b"]),
        ),
        (r"select substring('abc' from '('), substring('abc' from '[')", Err(r"2201B")),
        (
            r#"select substring('abcdef' similar 'abc' escape '#'), substring('abcdef' similar '%' escape '#'), substring('abc' similar 'a#"b#"c#"d' escape '#')"#,
            Err(r"2200C"),
        ),
        (r#"select substring('abcdef' similar '%#"cd#"%')"#, Err(r"42601")),
        (r#"select substring('abcdef' from '%#"cd#"%' for '')"#, Ok(&[r""])),
        (
            r"select substring('abc' from 2), substring('abc' from 2 for 1), substring('abc', 2), substring('abc', 2, 1), substring('abc' for 2), substring('abc' from 0 for 2)",
            Ok(&[r"bc|b|bc|b|ab|a"]),
        ),
        (r"select substring('a2c' from '2'), substring('abc' from '2')", Ok(&[r"2|"])),
        (
            r"select substring('héllo wörld' from 'w(.)r'), substring('HeLLo' from '(?i)l+'), substring('foo123bar' from '\d+'), substring('foo123bar' from '([a-z]+)(\d+)')",
            Ok(&[r"ö|LL|123|foo"]),
        ),
        (
            r"select substring(null from 'a'), substring('a' from null::text), substring('abc' similar null escape '#')",
            Ok(&[r"||"]),
        ),
        (r"select substring('abc'::bytea from 2), substring('abc'::bytea from 1 for 2)", Ok(&[r"\x6263|\x6162"])),
        (
            r"select id, substring(name from '^(.)'), substring(name from 'a$') from lk order by id",
            Ok(&[r"1|a|a", r"2|b|a", r"3|g|a", r"4||", r"5|d|a"]),
        ),
        (r"select id from lk where substring(name from '^(.)') = 'b'", Ok(&[r"2"])),
        (r"select substring('abc' from 'b' for '#')", Ok(&[r""])),
        (r"select substring('abc' similar 'b' escape '##')", Err(r"22025")),
    ]);
}

#[test]
fn regexp_functions_with_start_n_flags_and_string_to_table() {
    script(&[
        (
            r"select regexp_instr('abc123', '[0-9]+'), regexp_instr('abc123abc', 'abc', 2), regexp_instr('abcabc', 'abc', 1, 2), regexp_instr('abc123', '[0-9]+', 1, 1, 1), regexp_instr('abc', 'z')",
            Ok(&[r"4|7|4|7|0"]),
        ),
        (
            r"select regexp_instr('abcabc', '(b)(c)', 1, 1, 0, 'i', 2), regexp_instr('ABC', 'b', 1, 1, 0, 'i'), regexp_instr('abc', 'b', 1, 1, 1, 'c', 0)",
            Ok(&[r"3|2|3"]),
        ),
        (
            r"select regexp_substr('abc123', '[0-9]+'), regexp_substr('abcabc', 'abc', 2), regexp_substr('abcabc', 'abc', 1, 2), regexp_substr('ABC', 'b', 1, 1, 'i'), regexp_substr('abcd', '(b)(c)', 1, 1, '', 2), regexp_substr('abc','z')",
            Ok(&[r"123|abc|abc|B|c|"]),
        ),
        (
            r"select regexp_count('a1b2c3', '[0-9]'), regexp_count('a1b2c3', '[0-9]', 3), regexp_count('aAaA', 'a', 1, 'i'), regexp_count('abc', ''), regexp_count('', 'a')",
            Ok(&[r"3|2|4|4|0"]),
        ),
        (
            r"select regexp_replace('aaa', 'a', 'b', 2), regexp_replace('aaa', 'a', 'b', 2, 2), regexp_replace('aaa', 'a', 'b', 1, 0), regexp_replace('aAa', 'a', 'b', 1, 0, 'i'), regexp_replace('abcabc', '(b)', '[\1]', 2, 2), regexp_replace('abc','z','y',1,1)",
            Ok(&[r"aba|aab|bbb|bbb|abca[b]c|abc"]),
        ),
        (
            r"select regexp_replace('a1b22', '[0-9]+', '#', 'g'), regexp_replace('abc', '(b)', '[\1]')",
            Ok(&[r"a#b#|a[b]c"]),
        ),
        (r"select regexp_instr('abc', 'b', 0)", Err(r"22023")),
        (r"select regexp_substr('abc', 'b', 1, 0)", Err(r"22023")),
        (r"select regexp_count('abc', 'b', 0)", Err(r"22023")),
        (r"select regexp_instr('abc','b',1,1,2)", Err(r"22023")),
        (r"select regexp_instr(null,'a'), regexp_count('a',null), regexp_substr('a','a',null)", Ok(&[r"||"])),
        (r"select string_to_table('a,b,c', ',')", Ok(&[r"a", r"b", r"c"])),
        (r"select * from string_to_table('a,b,c', ',') with ordinality", Ok(&[r"a|1", r"b|2", r"c|3"])),
        (r"select string_to_table('a,b,c', ',', 'b')", Ok(&[r"a", r"", r"c"])),
        (r"select string_to_table('abc', '')", Ok(&[r"abc"])),
        (r"select string_to_table(null, ',')", Ok(&[])),
        (r"select string_to_table('abc', null)", Ok(&[r"a", r"b", r"c"])),
        (r"select upper(string_to_table('a,b', ',')), string_to_table('x,y', ',') || '!'", Ok(&[r"A|x!", r"B|y!"])),
        (r"select regexp_matches('a1b22', '[0-9]+', 'g')", Ok(&[r"{1}", r"{22}"])),
        (r"select (regexp_matches('a1b22', '([a-z])([0-9]+)'))[2]", Ok(&[r"1"])),
        (r"select regexp_matches('a1b22', '([a-z])([0-9]+)', 'g')", Ok(&[r"{a,1}", r"{b,22}"])),
        (r"select regexp_matches('abc', 'z'), 1", Ok(&[])),
    ]);
}
