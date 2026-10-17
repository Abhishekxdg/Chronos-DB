//! Postgres's regular expression syntax: the options, escapes, look around, back references,
//! classes and the choice of which of several matches to take. Postgres's answers are compared
//! in tests/pgdiff.rs (`regular_expression_syntax`); these expectations, all taken from a real
//! Postgres 16, run without one.

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

fn all(cases: &[(&str, &[&str])]) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, want) in cases {
        assert_eq!(&rows(&mut s, &db, q), want, "{q}");
    }
}

#[test]
fn options_choose_how_a_pattern_matches() {
    all(&[
        (
            r"select 'a' || chr(10) || 'b' ~ '(?n)^b', 'a' || chr(10) || 'b' ~ '(?p)^b', 'a' || chr(10) || 'b' ~ '(?w)^b'",
            &[r"t|f|t"],
        ),
        (
            r"select regexp_match('AbC', '(?i)abc'), regexp_match('AbC', '(?c)abc'), regexp_match('a b', '(?x) a \  b')",
            &[r#"{AbC}||{"a b"}"#],
        ),
        (
            r"select regexp_match('ab', '***:a.'), regexp_match('a.', '***=a.'), regexp_match('AB', '***:(?i)ab')",
            &[r"{ab}|{a.}|{AB}"],
        ),
        (
            r"select 'a' || chr(10) || 'b' ~ 'a.b', 'a' || chr(10) || 'b' ~ '(?n)a.b', 'a' || chr(10) || 'b' ~ '(?p)a.b', 'a' || chr(10) || 'b' ~ '(?w)a.b'",
            &[r"t|f|f|t"],
        ),
        (
            r"select 'a' || chr(10) || 'b' ~ '(?n)a\Db', 'a' || chr(10) || 'b' ~ '(?p)a\Wb', 'a' || chr(10) || 'b' ~ '(?n)a[^x]b', 'a' || chr(10) || 'b' ~ '(?p)a[^[:digit:]]b'",
            &[r"t|t|f|f"],
        ),
    ]);
}

#[test]
fn escapes_and_word_edges() {
    all(&[
        // \x takes every hex digit that follows, so this is a character no text here has
        (
            r"select regexp_match('aab', 'a\x61b'), regexp_match('aab', 'a\141b'), regexp_match('a.b', 'a\.b')",
            &["|{aab}|{a.b}"],
        ),
        (
            r"select regexp_match('foo bar', '\mbar\M'), regexp_match('foo bar', '\yfoo\y'), regexp_match('foobar', '\yfoo\y'), regexp_match('foobar', 'o\Yb'), regexp_match('foo bar', '\Afoo'), regexp_match('foo bar', 'bar\Z')",
            &[r"{bar}|{foo}||{ob}|{foo}|{bar}"],
        ),
        (
            r"select regexp_match('foo bar', '[[:<:]]bar'), regexp_match('foo bar', 'foo[[:>:]]'), regexp_match('a-b', '[a\-z]+'), regexp_match('ab', '[[.a.]][[=b=]]')",
            &[r"{bar}|{foo}|{a-}|{ab}"],
        ),
    ]);
}

#[test]
fn look_around() {
    all(&[
        (
            r"select regexp_match('ab', '(?<=a)b'), regexp_match('cb', '(?<!a)b'), regexp_match('ab', 'a(?=b)'), regexp_match('ac', 'a(?!b)'), regexp_match('ab', '(?=a)a')",
            &[r"{b}|{b}|{a}|{a}|{a}"],
        ),
        (
            r"select regexp_replace('a1b22c', '(?<=[a-z])\d+(?=[a-z])', '#', 'g'), regexp_replace('foobar', '(?<!foo)bar', 'X')",
            &[r"a#b#c|foobar"],
        ),
        (
            r"select regexp_match('ab', '(?=(a))a'), regexp_match('ab', 'a(?=(b))'), regexp_match('ab', '(?=(a))(a)'), regexp_match('ab', '(a)(?<=(a))')",
            &[r"{a}|{a}|{a}|{a}"],
        ),
    ]);
}

#[test]
fn back_references() {
    all(&[
        (
            r"select regexp_match('aa', '(a)\1'), regexp_match('abab', '(ab)\1'), regexp_match('aA', '(?i)(a)\1'), regexp_match('ab', '(a)\1')",
            &[r"{a}|{ab}|{a}|"],
        ),
        (
            r"select regexp_match('abcdefghijj', '(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)\10'), regexp_match('a' || chr(10), '(a)\12'), regexp_match('a' || chr(8), '(a)\10')",
            &[r"{a,b,c,d,e,f,g,h,i,j}|{a}|{a}"],
        ),
        (
            r"select regexp_match('B', '(x)?\D\1?'), regexp_match('xB', '(x)?\D\1?'), regexp_match('B', '(x)?\D(\1)?'), regexp_match('B', '(x)?\D(?:\1)?'), regexp_match('B', '(x)?\D\1{0}')",
            &[r"|{x}|{NULL,NULL}|{NULL}|{NULL}"],
        ),
        (
            r"select regexp_match('xBxx', '(x)?\D\1+'), regexp_match('xBx', '(x)?\D\1{2}'), regexp_match('xBxx', '(x)?\D\1{2}'), regexp_match('B', '(x)*\D\1*')",
            &[r"{x}||{x}|"],
        ),
        (r"select 'a' ~ '(a)|\1'", &[r"t"]),
        (r"select regexp_match('b', '(a)|\1?')", &[r""]),
        (r"select regexp_match('b', '(a)?\1')", &[r""]),
        (r"select regexp_match('ab', '(a)(?:b|\1)')", &[r"{a}"]),
        (r"select regexp_match('aa', '((a)\2)')", &[r"{aa,a}"]),
        (r"select regexp_match('aa', '(a)(\1)')", &[r"{a,a}"]),
        (
            r"select regexp_match('aaaa', '(a+)\1'), regexp_match('abcabc', '(a)(b)(c)\1\2\3'), regexp_match('ab', '(a)(b)\2')",
            &[r"{aa}|{a,b,c}|"],
        ),
        (r"select regexp_match('abc', '(?=(a))(?=(b))'), regexp_match('abc', '(?!(a))(b)')", &[r"|{b}"]),
        (
            r"select regexp_match('a', 'a{0}'), regexp_match('a', '(a){0}'), regexp_match('ab', '(a){0}b'), regexp_match('ab', '(a){0}(b)\2')",
            &[r#"{""}|{NULL}|{NULL}|"#],
        ),
        (r"select regexp_match('aa', '(a){0}\1')", &[r""]),
        (
            r"select regexp_match('xy', '(x)|y'), regexp_match('y', '(x)|y'), regexp_match('y', '(?:(x)|y)\1?')",
            &[r"{x}|{NULL}|"],
        ),
    ]);
}

#[test]
fn repeats_and_the_longest_match() {
    all(&[
        (
            r"select regexp_replace('1A1b B', '\w(x{2}|-{0,1}){1,2}', '<\&>', 'g'), regexp_match('b', '(a*)+'), regexp_match('b', '(a*)*'), regexp_match('b', '(a*){2}')",
            &[r#"<1><A><1><b> <B>|{""}|{""}|{""}"#],
        ),
        (
            r"select regexp_match('abcd', 'a|ab|abc'), regexp_match('abcd', '(a|ab)(c|bcd)'), regexp_match('abcd', '(a*?|ab)'), regexp_match('xyz', 'x*?|xy')",
            &[r"{abc}|{a,bcd}|{ab}|{xy}"],
        ),
        (
            r"select regexp_match('aaa', 'a*?'), regexp_match('aaa', 'a+?'), regexp_match('aaa', 'a{1,2}?'), regexp_match('abab', '(ab)*?b'), regexp_match('aaab', 'a*b?')",
            &[r#"{""}|{a}|{a}|{NULL}|{aaab}"#],
        ),
        (
            r"select regexp_replace('abcabc', 'b|bc', '<\&>', 'g'), regexp_replace('abc', 'a??', '-', 'g'), regexp_replace('xaay', 'a*?', '-', 'g')",
            &[r"a<bc>a<bc>|-a-b-c-|-x-a-a-y-"],
        ),
    ]);
}

#[test]
fn bracket_expressions_and_classes() {
    all(&[
        (
            r"select regexp_match('Été', '[[:upper:]][[:lower:]]+'), regexp_match('é1_', '[[:alnum:]_]+'), regexp_match('a-b', '[\w-]+'), regexp_match('a1', '[\d]+'), regexp_match(']', '[]a]'), regexp_match('b', '[^]a]')",
            &[r"{Été}|{é1_}|{a-b}|{1}|{]}|{b}"],
        ),
        (
            r"select regexp_match('a b', '[[:space:]]'), regexp_match('a,b', '[[:punct:]]'), regexp_match('xyz1', '[[:word:]]+'), regexp_match('aB', '[[:xdigit:]]+')",
            &[r#"{" "}|{","}|{xyz1}|{aB}"#],
        ),
        (
            r"select regexp_match('aaa', 'a{2}'), regexp_match('aaa', 'a{2,}'), regexp_match('a{,2}', 'a{,2}'), regexp_match('a{', 'a{'), regexp_match('a{x}', 'a{x}')",
            &[r#"{aa}|{aaa}|{"a{,2}"}|{"a{"}|{"a{x}"}"#],
        ),
        (r"select regexp_match('é', 'É'), regexp_match('É', '(?i)é'), regexp_match('ß', '(?i)SS')", &[r"|{É}|"]),
        (
            r"select regexp_match('foo', ''), regexp_match('', '^$'), regexp_match('ab', '()'), regexp_match('ab', '(?:)')",
            &[r#"{""}|{""}|{""}|{""}"#],
        ),
        (
            r"select '5' ~ '[\D]', 'a' ~ '[\D]', 'a' ~ '[^\D]', '5' ~ '[^\D]', 'a5' ~ '^[\Da]+$', ' ' ~ '[\S]', 'a' ~ '[\S]', '_' ~ '[\W]', 'a' ~ '[\W]', 'a' ~ '[\W\d]'",
            &[r"f|t|f|t|f|f|t|f|f|f"],
        ),
        (
            r"select regexp_match('a1 -', '[\D]+'), regexp_match('a1 -', '[^\W]+'), regexp_match('a1 -', '[\S]+'), regexp_match(chr(10), '(?n)[\D]') is not null, regexp_match(chr(10), '(?n)[^\d]') is not null",
            &[r"{a}|{a1}|{a1}|t|f"],
        ),
    ]);
}

#[test]
fn the_functions_take_them_too() {
    all(&[(
        r"select regexp_split_to_array('a1b22c', '\d+'), regexp_split_to_array('aXbxc', '(?i)x')",
        &[r"{a,b,c}|{a,b,c}"],
    )]);
}

#[test]
fn a_pattern_postgres_refuses_is_a_regex_error() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        r"select 'a' ~ 'a**'",
        r"select 'a' ~ '^*'",
        r"select 'a' ~ '$+'",
        r"select 'a' ~ '\y?'",
        r"select 'a' ~ '\m*'",
        r"select 'a' ~ '\A{2}'",
        r"select 'a' ~ '(?=a)*'",
        r"select 'a' ~ '(?!a)+'",
        r"select 'a' ~ '(?<=a){2}'",
        r"select 'a' ~ '[[:<:]]+'",
        r"select 'a' ~ 'a{2}{3}'",
        r"select 'a' ~ 'a*??'",
        r"select 'a' ~ '\1'",
        r"select 'a' ~ '(a)\2'",
        r"select 'a' ~ '(a\1)'",
        r"select 'a' ~ '(a)(?=\1)'",
        r"select 'a' ~ '(?<!(a)\1)a'",
        r"select 'a' ~ '(?=(a))\1'",
        r"select 'a' ~ '(a)\9'",
        r"select 'a' ~ '\8'",
        r"select 'a' ~ '(?z)'",
        r"select 'a' ~ 'a{3,2}'",
        r"select 'a' ~ '(a'",
        r"select 'a' ~ 'a)'",
        r"select 'a' ~ '[a'",
        r"select 'a' ~ '\'",
        r"select 'a' ~ '*a'",
        r"select 'a' ~ '[[:foo:]]'",
        r"select 'a' ~ 'a{2'",
        r"select 'a' ~ '(?<n>a)'",
        r"select regexp_replace('a', '(?=a)+', 'b')",
        r"select regexp_match('a', '(a)\2')",
    ] {
        let e = s.run(&db, q, &[]).expect_err(q);
        assert_eq!(e.code, "2201B", "{q}: {}", e.message);
    }
}
