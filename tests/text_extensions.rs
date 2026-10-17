//! pg_trgm, fuzzystrmatch and unaccent: their functions (`similarity`, `levenshtein`, `soundex`, `dmetaphone`,
//! `unaccent`, ...) and pg_trgm's operators (`%`, `<%`, `<->`, ...), which are built in, and `CREATE EXTENSION` for
//! them. The algorithms are in src/sql/textext.rs (checked against Postgres 16 on 138,000 generated cases);
//! Postgres's answers to these statements are compared in tests/pgdiff.rs (`text_extension_functions`,
//! `trigram_operators`); these run without one.

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
fn text_extension_functions_answer_as_postgres_does() {
    script(&[
        (r"create extension if not exists pg_trgm", Ok(&[])),
        (r"create extension if not exists fuzzystrmatch", Ok(&[])),
        (r"create extension if not exists unaccent", Ok(&[])),
        (
            r"select similarity('word','two words'), similarity('hello','hallo'), similarity('','abc'), similarity('abc','abc')",
            Ok(&[r"0.36363637|0.33333334|0|1"]),
        ),
        (
            r"select word_similarity('word','two words'), strict_word_similarity('word','two words'), word_similarity('cat','the big cat sat')",
            Ok(&[r"0.8|0.5714286|1"]),
        ),
        (
            r"select show_trgm('word'), show_trgm('two words'), show_trgm(''), show_trgm('a')",
            Ok(&[
                r#"{"  w"," wo",ord,"rd ",wor}|{"  t","  w"," tw"," wo","ds ",ord,rds,two,"wo ",wor}|{}|{"  a"," a "}"#,
            ]),
        ),
        (
            r"select show_trgm('Hello, World_1 ñandú')",
            Ok(&[
                r#"{0x927af7,0xb00338,0x00a9f5,0x12522c,"  1","  h","  w"," 1 "," he"," wo",0x304ab5,and,ell,hel,"ld ",llo,"lo ",orl,rld,wor}"#,
            ]),
        ),
        (r"select similarity(null,'a'), word_similarity('a',null), show_trgm(null)", Ok(&[r"||"])),
        (
            r"select levenshtein('kitten','sitting'), levenshtein('','abc'), levenshtein('abc',''), levenshtein('same','same')",
            Ok(&[r"3|3|3|0"]),
        ),
        (r"select levenshtein('kitten','sitting',1,2,3), levenshtein('kitten','sitting',2,1,1)", Ok(&[r"7|4"])),
        (
            r"select levenshtein_less_equal('kitten','sitting',2), levenshtein_less_equal('kitten','sitting',3), levenshtein_less_equal('extensive','exhaustive',2), levenshtein_less_equal('extensive','exhaustive',4)",
            Ok(&[r"3|3|3|4"]),
        ),
        (r"select levenshtein_less_equal('kitten','sitting',1,2,3,5)", Ok(&[r"6"])),
        // costs so large that the first row's stop column wraps negative: the band is empty and the answer is
        // max_d + 1, where the same costs without the bound give the wrapped distance (Postgres 16.13's answers)
        (
            r"select levenshtein_less_equal('abba','babbabb',715827884,3,715827892,5), levenshtein('abba','babbabb',715827884,3,715827892)",
            Ok(&[r"6|-1431655757"]),
        ),
        (
            r"select levenshtein_less_equal('aba','baaaaaa',536870912,536869912,2103821310,2), levenshtein_less_equal('aab','bbbab',1073741824,-683094506,-8,9)",
            Ok(&[r"3|10"]),
        ),
        (r"select levenshtein(repeat('a',300),'b')", Err(r"22023")),
        (r"select levenshtein('kitten', 'sitting', -1, 1, 1)", Ok(&[r"-1"])),
        (
            r"select soundex('Robert'), soundex('Rupert'), soundex('Tymczak'), soundex(''), soundex('123'), soundex('Ashcraft')",
            Ok(&[r"R163|R163|T522|||A226"]),
        ),
        (r"select difference('Anne','Ann'), difference('Anne','Andrew'), difference('','')", Ok(&[r"4|2|4"])),
        (
            r"select metaphone('Thompson',10), metaphone('Knight',4), metaphone('Schmidt',6), metaphone('',5)",
            Ok(&[r"0MPSN|NFT|SKMTT|"]),
        ),
        (r"select metaphone('abc',0)", Err(r"2200F")),
        (r"select metaphone('abc',-1)", Err(r"2200F")),
        (r"select metaphone(repeat('a',300),5)", Err(r"22023")),
        (
            r"select dmetaphone('Smith'), dmetaphone('Schmidt'), dmetaphone_alt('Smith'), dmetaphone_alt('Schmidt'), dmetaphone('Jose'), dmetaphone_alt('Jose')",
            Ok(&[r"SM0|XMT|XMT|SMT|HS|HS"]),
        ),
        (
            r"select dmetaphone('Xavier'), dmetaphone('Thomas'), dmetaphone('Caesar'), dmetaphone('Wright')",
            Ok(&[r"SF|TMS|SSR|RT"]),
        ),
        (
            r"select daitch_mokotoff('Schwartz'), daitch_mokotoff('Moskowitz'), daitch_mokotoff('Peterson'), daitch_mokotoff('')",
            Ok(&[r"{479400}|{645740}|{739460,734600}|"]),
        ),
        (
            r"select unaccent('Hôtel éléphant'), unaccent('ÀÉÎÕÜ àéîõü ñ ß æ'), unaccent(''), unaccent('plain')",
            Ok(&[r"Hotel elephant|AEIOU aeiou n ss ae||plain"]),
        ),
        (r"select unaccent('unaccent', 'Hôtel'), unaccent('public.unaccent', 'Hôtel')", Ok(&[r"Hotel|Hotel"])),
        (r"select unaccent('nosuchdict', 'Hôtel')", Err(r"42704")),
        (r"select unaccent(null), soundex(null), levenshtein(null,'a'), difference('a',null)", Ok(&[r"|||"])),
        (
            r"select pg_typeof(levenshtein('a','b')), pg_typeof(soundex('a')), pg_typeof(daitch_mokotoff('a')), pg_typeof(unaccent('a'))",
            Ok(&[r"integer|text|text[]|text"]),
        ),
        (r"create table trg (id serial primary key, name text)", Ok(&[])),
        (
            r"insert into trg(name) values ('Hôtel Paris'),('Hotel Berlin'),('Motel One'),('hostel'),('Grand Hotel')",
            Ok(&[]),
        ),
        (r"create index trg_name_gin on trg using gin (name gin_trgm_ops)", Ok(&[])),
        (r"create index trg_name_gist on trg using gist (name gist_trgm_ops)", Ok(&[])),
        (
            r"select name, similarity(name,'hotel') s from trg order by s desc, name",
            Ok(&[
                r"Grand Hotel|0.5",
                r"Hotel Berlin|0.46153846",
                r"hostel|0.44444445",
                r"Motel One|0.23076923",
                r"Hôtel Paris|0.2",
            ]),
        ),
        (
            r"select name from trg where unaccent(name) ilike '%hotel%' order by name",
            Ok(&[r"Grand Hotel", r"Hotel Berlin", r"Hôtel Paris"]),
        ),
        (
            r"select name, levenshtein(lower(name),'hotel') d from trg order by d, name",
            Ok(&[r"hostel|1", r"Motel One|5", r"Grand Hotel|6", r"Hotel Berlin|7", r"Hôtel Paris|7"]),
        ),
        (
            r"select name, soundex(name) from trg order by name",
            Ok(&[r"Grand Hotel|G653", r"Hotel Berlin|H341", r"Hôtel Paris|H341", r"Motel One|M345", r"hostel|H234"]),
        ),
        (
            r"select name from trg where difference(name,'hotel') >= 3 order by name",
            Ok(&[r"Hotel Berlin", r"Hôtel Paris"]),
        ),
        (
            r"select pg_typeof(show_trgm('a')), pg_typeof(levenshtein('a','b')), pg_typeof(soundex('a')), pg_typeof(daitch_mokotoff('a')), pg_typeof(unaccent('a'))",
            Ok(&[r"text[]|integer|text|text[]|text"]),
        ),
        (r"drop extension if exists fuzzystrmatch, unaccent", Ok(&[])),
        (r"drop extension pg_trgm cascade", Ok(&[])),
        (r"drop extension if exists pg_trgm", Ok(&[])),
    ]);
}

#[test]
fn trigram_operators_answer_as_postgres_does() {
    script(&[
        (r"create extension if not exists pg_trgm", Ok(&[])),
        (r"select 'word' % 'two words', 'hello' % 'hallo', 'abc' % 'xyz', 'hotel' % 'hotell'", Ok(&[r"t|t|f|t"])),
        (r"select 'word' <% 'two words', 'cat' <% 'the big cat sat', 'dog' <% 'the big cat sat'", Ok(&[r"t|t|f"])),
        (r"select 'two words' %> 'word', 'the big cat sat' %> 'cat', 'the big cat sat' %> 'dog'", Ok(&[r"t|t|f"])),
        (r"select 'word' <<% 'two words', 'cat' <<% 'the big cat sat', 'wo' <<% 'two words'", Ok(&[r"t|t|f"])),
        (r"select 'two words' %>> 'word', 'the big cat sat' %>> 'cat'", Ok(&[r"t|t"])),
        (
            r"select 'word' <-> 'two words', 'hello' <-> 'hallo', 'abc' <-> 'abc', 'abc' <-> 'xyz'",
            Ok(&[r"0.6363636|0.6666666|0|1"]),
        ),
        (
            r"select 'word' <<-> 'two words', 'cat' <<-> 'the big cat sat', 'two words' <->> 'word'",
            Ok(&[r"0.19999999|0|0.19999999"]),
        ),
        (r"select 'word' <<<-> 'two words', 'two words' <->>> 'word'", Ok(&[r"0.4285714|0.4285714"])),
        (r"select null % 'a', 'a' % null, null <-> 'a', null <% 'a'", Ok(&[r"|||"])),
        (r"select 7 % 3, 7.5 % 2, (-7) % 3", Ok(&[r"1|1.5|-1"])),
        (r"create table trg2 (id serial primary key, name text, score int)", Ok(&[])),
        (
            r"insert into trg2(name, score) values ('Hotel Paris', 3),('Hotel Berlin', 5),('Motel One', 7),('hostel', 9),('Grand Hotel', 11),('Paris Hilton', 13)",
            Ok(&[]),
        ),
        (r"create index trg2_gin on trg2 using gin (name gin_trgm_ops)", Ok(&[])),
        (r"select name from trg2 where name % 'hotle' order by name", Ok(&[])),
        (
            r"select name from trg2 where name % 'hotel' order by name",
            Ok(&[r"Grand Hotel", r"Hotel Berlin", r"Hotel Paris", r"hostel"]),
        ),
        (
            r"select name, name <-> 'hotel' d from trg2 order by name <-> 'hotel', name",
            Ok(&[
                r"Grand Hotel|0.5",
                r"Hotel Paris|0.5",
                r"Hotel Berlin|0.53846157",
                r"hostel|0.5555556",
                r"Motel One|0.7692308",
                r"Paris Hilton|0.9444444",
            ]),
        ),
        (r"select name from trg2 where name <% 'paris' order by name", Ok(&[])),
        (r"select name from trg2 where 'paris hotel' %> name order by name", Ok(&[r"Hotel Paris"])),
        (
            r"select name, round((1 - (name <-> 'hotel'))::numeric, 3) from trg2 order by 2 desc, name limit 3",
            Ok(&[r"Grand Hotel|0.500", r"Hotel Paris|0.500", r"Hotel Berlin|0.462"]),
        ),
        (
            r"select name from trg2 where score % 2 = 1 and name % 'hotel' order by name",
            Ok(&[r"Grand Hotel", r"Hotel Berlin", r"Hotel Paris", r"hostel"]),
        ),
        (
            r"select a.name, b.name from trg2 a join trg2 b on a.name % b.name and a.id < b.id order by 1, 2",
            Ok(&[
                r"Hotel Berlin|Grand Hotel",
                r"Hotel Paris|Grand Hotel",
                r"Hotel Paris|Hotel Berlin",
                r"Hotel Paris|Paris Hilton",
            ]),
        ),
        (
            r"select name from trg2 where name ilike '%otel%' and name % 'hotel' order by name",
            Ok(&[r"Grand Hotel", r"Hotel Berlin", r"Hotel Paris"]),
        ),
        (r"select name % 'hotel' from trg2 order by id", Ok(&[r"t", r"t", r"f", r"t", r"t", r"f"])),
        (r"select count(*) from trg2 where similarity(name, 'hotel') > 0.3", Ok(&[r"4"])),
    ]);
}

/// Texts too long for Postgres's allocator, which refuses them: pg_trgm from about 238 MB (a text, or two that add
/// up to it), and with 134,217,728 trigrams for word similarity; the same errors Postgres 16.13 gave for
/// `repeat(' ', n)`. "out of memory" is 54000, but "invalid memory alloc request size" is XX000.
#[test]
fn long_texts_are_refused_as_postgres_refuses_them() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let mut refused = |q: &str, code: &str, message: &str| {
        let e = s.run(&db, q, &[]).expect_err(q);
        assert_eq!((e.code, e.message.as_str()), (code, message), "{q}");
    };
    let (oom, size) = ("out of memory", "invalid memory alloc request size 1073741828");
    refused("select similarity(repeat(' ', 238609294), 'a')", "54000", oom);
    refused("select similarity('a', repeat(' ', 238609293))", "XX000", size);
    refused("select show_trgm(repeat(' ', 238609292))", "XX000", size);
    refused("select repeat(' ', 238609292) <-> repeat(' ', 238609294)", "XX000", size);
    refused("select repeat(' ', 238609294) % repeat(' ', 238609292)", "54000", oom);
    // word similarity adds the two lengths: 238609292 and one byte is fine, and two bytes are too many
    refused("select word_similarity('aa', repeat(' ', 238609292))", "54000", oom);
    refused("select repeat(' ', 238609292) %>> 'aa'", "54000", oom);
}
