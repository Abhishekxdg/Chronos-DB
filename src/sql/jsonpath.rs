//! SQL/JSON path, the `jsonpath` language of PostgreSQL 16 (docs 9.16.2): `lax`/`strict` modes,
//! `$` `@` `$var`, member and array accessors, `.*` `[*]` `.**`, filters `? (...)`, predicates
//! (comparisons, `&&` `||` `!`, `is unknown`, `exists`, `like_regex`, `starts with`), arithmetic and
//! the item methods `.type() .size() .double() .ceiling() .floor() .abs() .keyvalue() .datetime()`.
//!
//! `parse` reads a path as Postgres's scanner and grammar do (same error texts; `parse_code` adds
//! the SQLSTATE, which is 42601 for a syntax error but not always), `query_value` walks a jsonb
//! value the way `jsonb_path_query_array` does (with `tz` set, `jsonb_path_query_array_tz`),
//! `exists_value` and `predicate_value` are `jsonb_path_exists` and `jsonb_path_match`, which are
//! what `@?` and `@@` return when called with no variables (an empty object) and `silent` set. A
//! `Path` prints as `jsonpath` does (`Display`). The walk follows jsonpath_exec.c step by step,
//! because lax mode's auto-wrapping and unwrapping and what `silent` suppresses are the details
//! that make the same path answer differently.
//!
//! A number is a `Numeric`: an exact decimal with Postgres's limits (under 10^131072, at most 16383
//! digits after the point), its display scale (`1.50`, `0.5 * 4` is `2.0`, `10 / 4` is
//! `2.5000000000000000`) and its rounding, nothing passing through a float between one step of a
//! path and the next. A `Value` is a jsonb value that holds such numbers (`Value::parse` reads JSON
//! text without losing a digit, `Display` writes it as jsonb does). `query`, `exists` and
//! `predicate` take `serde_json` values instead, which hold floats: a target or a result is as
//! exact as a float is, and a result past what a float holds is an error.
//! `.datetime()` reads a string by the standard-mode templates of `to_timestamp` (`datetime.rs`)
//! and compares in the session's time zone (`time::ZONE`) where the `_tz` functions may. `like_regex`
//! runs on a port of Postgres's own regular expression syntax (`are.rs`: AREs, EREs, BREs, lookaround,
//! back-references, C.UTF-8 character classes), flag `x` refused as Postgres refuses it.
//! ponytail: paths nest up to 50000 levels and have 200000 items (Postgres stops at a few thousand,
//! by stack depth), where a thread with a stack of its own takes what the caller's has no room for;
//! a regular expression is "too complex" at a size of its own here, not Postgres's; a walk that
//! holds more than 2 GiB of results (`$.**.**.**...` over a big document) or takes 2^31 steps is
//! refused with 54000, where Postgres runs out of memory or goes on until it is cancelled; the
//! methods Postgres 17 adds (`.bigint()`, `.string()`, ...) are not here.

// (the SQL functions and operators call `query_value`, `exists_value` and `predicate_value`; the rest of the
// module's API is for its own tests)
#![allow(dead_code)]

mod are;
mod ctype;
mod datetime;
mod num;
mod value;

// what the functions take and give
#[allow(unused_imports)]
pub use datetime::Datetime;
#[allow(unused_imports)]
pub use num::Numeric;
#[allow(unused_imports)]
pub use value::{Object, Value};

use num::NumError;
use serde_json::Value as Json;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// A SQLSTATE and its message, as a failing path reports them.
pub type PathError = (&'static str, String);

/// A parsed path: its mode and its expression.
#[derive(Clone)]
pub struct Path {
    lax: bool,
    expr: Arc<Node>,
    /// how many items it has, which says whether printing it needs a stack of its own
    nodes: usize,
}

impl fmt::Debug for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Path({self})")
    }
}

// ---------------------------------------------------------------------------------------------
// numbers

/// The literal a number token spells: a decimal, or an integer in another base. Err is numeric
/// overflow (Postgres's limits: under 10^131072, at most 16383 digits after the point).
fn literal(t: &NumTok) -> Result<Numeric, PathError> {
    let n = if t.radix == 10 { Numeric::parse(&t.text) } else { Numeric::from_radix(&t.text, t.radix) };
    n.map_err(NumError::err)
}

// ---------------------------------------------------------------------------------------------
// the syntax tree

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ar {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Type,
    Size,
    Abs,
    Floor,
    Ceiling,
    Double,
    KeyValue,
}

/// One item of a path, and the accessor chain that goes on from it.
struct Node {
    op: Op,
    next: Option<Box<Node>>,
}

impl Node {
    /// An item with nothing in it: what a node's parts are swapped for when it is taken apart.
    fn hollow() -> Node {
        Node { op: Op::Null, next: None }
    }

    /// Moves the items this one holds onto `out`.
    fn detach(&mut self, out: &mut Vec<Node>) {
        out.extend(self.next.take().map(|n| *n));
        let take = |n: &mut Box<Node>| std::mem::replace(&mut **n, Node::hollow());
        match &mut self.op {
            Op::Filter(a) | Op::Plus(a) | Op::Minus(a) | Op::Not(a) | Op::IsUnknown(a) | Op::Exists(a) => {
                out.push(take(a));
            }
            Op::LikeRegex(a, ..) => out.push(take(a)),
            Op::Bin(_, a, b) | Op::Cmp(_, a, b) | Op::And(a, b) | Op::Or(a, b) | Op::StartsWith(a, b) => {
                out.push(take(a));
                out.push(take(b));
            }
            Op::Index(elems) => {
                for (from, to) in std::mem::take(elems) {
                    out.push(from);
                    out.extend(to);
                }
            }
            _ => {}
        }
    }
}

impl Drop for Node {
    /// Taken apart with a list, not by recursion: a path may be a hundred thousand items deep,
    /// and dropped on a thread with little stack.
    fn drop(&mut self) {
        let mut stack = vec![];
        self.detach(&mut stack);
        while let Some(mut n) = stack.pop() {
            n.detach(&mut stack);
        }
    }
}

enum Op {
    Null,
    Bool(bool),
    Num(Numeric),
    Str(String),
    Var(String),
    Root,
    Current,
    Last,
    Key(String),
    AnyKey,
    AnyArray,
    /// `[a, b to c]`
    Index(Vec<(Node, Option<Node>)>),
    /// `.**{first to last}`; u32::MAX is `last`
    Any(u32, u32),
    Filter(Box<Node>),
    Bin(Ar, Box<Node>, Box<Node>),
    Plus(Box<Node>),
    Minus(Box<Node>),
    Cmp(Cmp, Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
    IsUnknown(Box<Node>),
    Exists(Box<Node>),
    StartsWith(Box<Node>, Box<Node>),
    LikeRegex(Box<Node>, String, u8, Arc<are::Regex>),
    Method(Method),
    Datetime(Option<String>),
}

const RE_I: u8 = 1;
const RE_S: u8 = 2;
const RE_M: u8 = 4;
const RE_X: u8 = 8;
const RE_Q: u8 = 16;

// ---------------------------------------------------------------------------------------------
// the scanner (jsonpath_scan.l)

#[derive(Clone, Debug, PartialEq)]
struct NumTok {
    /// the digits, with no underscores and no 0x prefix
    text: String,
    radix: u32,
    /// an integer literal (INT_P), as `.**{n}` wants
    int: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum T {
    Eof,
    /// the scanner failed: a SQLSTATE and a message
    Fail(&'static str, String),
    And,
    Or,
    Not,
    Any,
    Lt,
    Le,
    Eq,
    Ne,
    Gt,
    Ge,
    Ch(char),
    Str(String),
    Var(String),
    Word(String),
    Num(NumTok),
}

/// A token and the `yytext` Postgres's scanner holds once it has returned it, which is what
/// "at or near" quotes: for a word it is what ended the word (blanks, or nothing).
#[derive(Clone, Debug)]
struct Tok {
    t: T,
    text: String,
}

fn is_special(c: char) -> bool {
    "?%$.[]{}()|&!=<>@#,*:-+/\\\" \t\n\r\x0c".contains(c)
}

fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

/// A scanner error at `text` (what the scanner last matched; nothing at the end of the input).
fn fail(msg: &str, text: &str) -> T {
    T::Fail(
        "42601",
        if text.is_empty() {
            format!("{msg} at end of jsonpath input")
        } else {
            format!("{msg} at or near \"{text}\" of jsonpath input")
        },
    )
}

fn hard(code: &'static str, msg: &str) -> T {
    T::Fail(code, msg.into())
}

struct Lexer {
    c: Vec<char>,
    i: usize,
}

impl Lexer {
    fn at(&self, k: usize) -> Option<char> {
        self.c.get(self.i + k).copied()
    }

    fn tok(t: T, text: &str) -> Tok {
        Tok { t, text: text.into() }
    }

    fn fail_tok(t: T) -> Tok {
        Tok { t, text: String::new() }
    }

    fn all(src: &str) -> Vec<Tok> {
        let mut lx = Lexer { c: src.chars().collect(), i: 0 };
        let mut out = vec![];
        loop {
            let t = lx.next();
            let end = matches!(t.t, T::Eof | T::Fail(..));
            out.push(t);
            if end {
                return out;
            }
        }
    }

    fn next(&mut self) -> Tok {
        loop {
            let Some(ch) = self.at(0) else { return Lexer::tok(T::Eof, "") };
            if is_blank(ch) {
                while self.at(0).is_some_and(is_blank) {
                    self.i += 1;
                }
                continue;
            }
            if ch == '/' && self.at(1) == Some('*') {
                self.i += 2;
                if let Err(t) = self.comment() {
                    return Lexer::fail_tok(t);
                }
                continue;
            }
            let two = |lx: &mut Lexer, second: char, yes: T, text: &str, no: T, text1: &str| {
                if lx.at(1) == Some(second) {
                    lx.i += 2;
                    Lexer::tok(yes, text)
                } else {
                    lx.i += 1;
                    Lexer::tok(no, text1)
                }
            };
            return match ch {
                '&' => two(self, '&', T::And, "&&", T::Ch('&'), "&"),
                '|' => two(self, '|', T::Or, "||", T::Ch('|'), "|"),
                '!' => two(self, '=', T::Ne, "!=", T::Not, "!"),
                '*' => two(self, '*', T::Any, "**", T::Ch('*'), "*"),
                '=' => two(self, '=', T::Eq, "==", T::Ch('='), "="),
                '>' => two(self, '=', T::Ge, ">=", T::Gt, ">"),
                '<' => match self.at(1) {
                    Some('=') => {
                        self.i += 2;
                        Lexer::tok(T::Le, "<=")
                    }
                    Some('>') => {
                        self.i += 2;
                        Lexer::tok(T::Ne, "<>")
                    }
                    _ => {
                        self.i += 1;
                        Lexer::tok(T::Lt, "<")
                    }
                },
                '"' => {
                    self.i += 1;
                    match self.quoted() {
                        Ok(s) => Lexer::tok(T::Str(s), "\""),
                        Err(t) => Lexer::fail_tok(t),
                    }
                }
                '$' => match self.at(1) {
                    Some('"') => {
                        self.i += 2;
                        match self.quoted() {
                            Ok(s) => Lexer::tok(T::Var(s), "\""),
                            Err(t) => Lexer::fail_tok(t),
                        }
                    }
                    Some(c) if !is_special(c) => {
                        self.i += 1;
                        let from = self.i;
                        while self.at(0).is_some_and(|c| !is_special(c)) {
                            self.i += 1;
                        }
                        let name: String = self.c[from..self.i].iter().collect();
                        Lexer::tok(T::Var(name.clone()), &format!("${name}"))
                    }
                    _ => {
                        self.i += 1;
                        Lexer::tok(T::Ch('$'), "$")
                    }
                },
                '\\' => match self.word() {
                    Some(t) => t,
                    None => continue,
                },
                '.' if self.at(1).is_some_and(|c| c.is_ascii_digit()) => match self.number_or_word() {
                    Some(t) => t,
                    None => continue,
                },
                c if is_special(c) => {
                    self.i += 1;
                    Lexer::tok(T::Ch(c), &c.to_string())
                }
                _ => match self.number_or_word() {
                    Some(t) => t,
                    None => continue,
                },
            };
        }
    }

    /// Past `/*`: to the end of the comment.
    fn comment(&mut self) -> Result<(), T> {
        while self.i < self.c.len() {
            if self.c[self.i] == '*' && self.at(1) == Some('/') {
                self.i += 2;
                return Ok(());
            }
            self.i += 1;
        }
        Err(fail("unexpected end of comment", ""))
    }

    /// `[d](_?[d])*` from `j`: where it ends (`j` if there's no digit).
    fn run(&self, j: usize, ok: fn(char) -> bool) -> usize {
        if !self.c.get(j).is_some_and(|&c| ok(c)) {
            return j;
        }
        let mut k = j + 1;
        loop {
            if self.c.get(k).is_some_and(|&c| ok(c)) {
                k += 1;
            } else if self.c.get(k) == Some(&'_') && self.c.get(k + 1).is_some_and(|&c| ok(c)) {
                k += 2;
            } else {
                return k;
            }
        }
    }

    /// `0 | [1-9](_?[0-9])*`
    fn decinteger(&self, j: usize) -> usize {
        match self.c.get(j) {
            Some('0') => j + 1,
            Some('1'..='9') => self.run(j, |c| c.is_ascii_digit()),
            _ => j,
        }
    }

    /// `{decinteger}\.{decdigits}? | \.{decdigits}`
    fn decimal(&self, j: usize) -> Option<usize> {
        let ie = self.decinteger(j);
        if ie > j && self.c.get(ie) == Some(&'.') {
            let de = self.run(ie + 1, |c| c.is_ascii_digit());
            Some(de.max(ie + 1))
        } else if self.c.get(j) == Some(&'.') {
            let de = self.run(j + 1, |c| c.is_ascii_digit());
            (de > j + 1).then_some(de)
        } else {
            None
        }
    }

    /// The longest number-like rule at `j`, as (length in characters, rule number). The scanner
    /// works on bytes, so "longest" counts those: a number followed by a multibyte character
    /// is a word, but followed by an ASCII one, it is trailing junk. Rules in the scanner's
    /// order decide ties.
    fn number(&self, j: usize) -> (usize, usize) {
        let other = |k: usize| self.c.get(k).is_some_and(|&c| !is_special(c));
        let bytes = |end: usize| self.c[j..end].iter().map(|c| c.len_utf8()).sum::<usize>();
        let mut best = (0, 0, usize::MAX); // bytes, characters, rule
        let mut offer = |end: usize, rule: usize, junk: bool| {
            if end > j {
                let (b, n) = if junk { (bytes(end) + 1, end - j + 1) } else { (bytes(end), end - j) };
                if b > best.0 {
                    best = (b, n, rule);
                }
            }
        };
        let dec = self.decimal(j);
        let int = self.decinteger(j);
        if let Some(e) = dec {
            offer(e, 0, false);
        }
        // real (and its failure: an exponent sign and no digits)
        let (mut real, mut realfail) = (j, j);
        for base in [dec.unwrap_or(j), int] {
            if base > j && matches!(self.c.get(base), Some('e' | 'E')) {
                let s = base + 1 + usize::from(matches!(self.c.get(base + 1), Some('+' | '-')));
                let de = self.run(s, |c| c.is_ascii_digit());
                if de > s {
                    real = real.max(de);
                } else if s > base + 1 {
                    realfail = realfail.max(s);
                }
            }
        }
        offer(real, 1, false);
        offer(int, 2, false);
        let radix = |lx: &Lexer, marks: [char; 2], ok: fn(char) -> bool| {
            let end = lx.run(j + 2, ok);
            let marked = lx.c.get(j) == Some(&'0') && lx.c.get(j + 1).is_some_and(|c| *c == marks[0] || *c == marks[1]);
            if marked && end > j + 2 { end } else { j }
        };
        offer(radix(self, ['x', 'X'], |c| c.is_ascii_hexdigit()), 3, false);
        offer(radix(self, ['o', 'O'], |c| ('0'..='7').contains(&c)), 4, false);
        offer(radix(self, ['b', 'B'], |c| c == '0' || c == '1'), 5, false);
        // a number straight followed by a character a word could hold: trailing junk
        if let Some(e) = dec.filter(|&e| other(e)) {
            offer(e, 6, true);
        }
        if real > j && other(real) {
            offer(real, 7, true);
        }
        if int > j && other(int) {
            offer(int, 8, true);
        }
        offer(realfail, 9, false);
        if other(j) {
            let mut k = j;
            while other(k) {
                k += 1;
            }
            offer(k, 10, false);
        }
        (best.1, best.2)
    }

    /// A number, or a word; None if a comment swallowed the word.
    fn number_or_word(&mut self) -> Option<Tok> {
        let (len, rule) = self.number(self.i);
        let text: String = self.c[self.i..self.i + len].iter().collect();
        match rule {
            0..=5 => {
                self.i += len;
                let clean: String = text.chars().filter(|&c| c != '_').collect();
                let (t, radix) = match rule {
                    3 => (clean[2..].to_string(), 16),
                    4 => (clean[2..].to_string(), 8),
                    5 => (clean[2..].to_string(), 2),
                    _ => (clean, 10),
                };
                Some(Lexer::tok(T::Num(NumTok { text: t, radix, int: matches!(rule, 2..=5) }), &text))
            }
            6..=8 => {
                // the scanner matches one byte past the number: half a multibyte character
                // is dropped from the message
                let mut shown = text.as_str();
                if let Some(c) = shown.chars().last().filter(|c| !c.is_ascii()) {
                    shown = &shown[..shown.len() - c.len_utf8()];
                }
                Some(Lexer::fail_tok(fail("trailing junk after numeric literal", shown)))
            }
            9 => Some(Lexer::fail_tok(fail("invalid numeric literal", &text))),
            _ => self.word(),
        }
    }

    /// An unquoted word (or a key): escapes are read, and it ends at a blank (which then is the
    /// text Postgres quotes), a special character or the end (nothing). A comment right after
    /// it swallows it (None), as the scanner does.
    fn word(&mut self) -> Option<Tok> {
        let mut s = String::new();
        loop {
            let Some(ch) = self.at(0) else { return Some(Lexer::tok(T::Word(s), "")) };
            if is_blank(ch) {
                let from = self.i;
                while self.at(0).is_some_and(is_blank) {
                    self.i += 1;
                }
                let blanks: String = self.c[from..self.i].iter().collect();
                return Some(Lexer::tok(T::Word(s), &blanks));
            }
            if ch == '/' && self.at(1) == Some('*') {
                self.i += 2;
                return match self.comment() {
                    Ok(()) => None,
                    Err(t) => Some(Lexer::fail_tok(t)),
                };
            }
            if ch == '\\' {
                if let Err(t) = self.escape(&mut s) {
                    return Some(Lexer::fail_tok(t));
                }
            } else if is_special(ch) {
                return Some(Lexer::tok(T::Word(s), ""));
            } else {
                s.push(ch);
                self.i += 1;
            }
        }
    }

    /// Past the opening quote: the string, read to its closing quote.
    fn quoted(&mut self) -> Result<String, T> {
        let mut s = String::new();
        loop {
            match self.at(0) {
                None => return Err(fail("unterminated quoted string", "")),
                Some('"') => {
                    self.i += 1;
                    return Ok(s);
                }
                Some('\\') => self.escape(&mut s)?,
                Some(c) => {
                    s.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// The escape at `self.i` (a backslash), added to `s`.
    fn escape(&mut self, s: &mut String) -> Result<(), T> {
        let hex = |c: Option<char>| c.and_then(|c| c.to_digit(16));
        match self.at(1) {
            None | Some('\n') => Err(fail("unexpected end after backslash", "\\")),
            Some('x') => match (hex(self.at(2)), hex(self.at(3))) {
                (Some(a), Some(b)) => {
                    self.i += 4;
                    add_char(a * 16 + b, s)
                }
                (a, _) => {
                    let mut text = String::from("\\x");
                    if a.is_some() {
                        text.push(self.c[self.i + 2]);
                    }
                    Err(fail("invalid hexadecimal character sequence", &text))
                }
            },
            Some('u') => self.unicode(s),
            Some(c) => {
                self.i += 2;
                s.push(match c {
                    'b' => '\x08',
                    'f' => '\x0c',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'v' => '\x0b',
                    c => c,
                });
                Ok(())
            }
        }
    }

    /// One `\uXXXX` or `\u{X...}` at `j`: its code and where it ends.
    fn unicode_at(&self, j: usize) -> Option<(u32, usize)> {
        if self.c.get(j) != Some(&'\\') || self.c.get(j + 1) != Some(&'u') {
            return None;
        }
        let hex = |k: usize| self.c.get(k).and_then(|c| c.to_digit(16));
        if self.c.get(j + 2) == Some(&'{') {
            let (mut k, mut v) = (j + 3, 0u32);
            while let Some(d) = hex(k) {
                v = v << 4 | d;
                k += 1;
            }
            (k > j + 3 && k - j - 3 <= 6 && self.c.get(k) == Some(&'}')).then_some((v, k + 1))
        } else {
            let mut v = 0;
            for k in j + 2..j + 6 {
                v = v << 4 | hex(k)?;
            }
            Some((v, j + 6))
        }
    }

    /// The `\u` that is not an escape at `j`, as far as the scanner's `unicodefail` reads it: `\u`
    /// and up to three hex digits, or `\u{` and up to six.
    fn unicode_fail_at(&self, j: usize) -> String {
        let mut text = String::from("\\u");
        let mut k = j + 2;
        let brace = self.c.get(k) == Some(&'{');
        if brace {
            text.push('{');
            k += 1;
        }
        let most = if brace { 6 } else { 3 };
        while text.chars().count() < 2 + usize::from(brace) + most
            && self.c.get(k).is_some_and(|c| c.is_ascii_hexdigit())
        {
            text.push(self.c[k]);
            k += 1;
        }
        text
    }

    /// A run of `\u` escapes, which may hold a surrogate pair. The scanner reads the whole run
    /// at once (`{unicode}+`), or, if a `\u` that is no escape follows it, the run and that
    /// (`{unicode}*{unicodefail}`): so that is the text an error quotes.
    fn unicode(&mut self, s: &mut String) -> Result<(), T> {
        // (what follows `\u{1}` is the DETAIL: see `path_error`)
        let high_high =
            "invalid input syntax for type jsonpath\u{1}Unicode high surrogate must not follow a high surrogate.";
        let no_high = "invalid input syntax for type jsonpath\u{1}Unicode low surrogate must follow a high surrogate.";
        let start = self.i;
        let mut codes = vec![];
        let mut j = start;
        while let Some((ch, end)) = self.unicode_at(j) {
            codes.push(ch);
            j = end;
        }
        if self.c.get(j) == Some(&'\\') && self.c.get(j + 1) == Some(&'u') {
            let mut text: String = self.c[start..j].iter().collect();
            text.push_str(&self.unicode_fail_at(j));
            return Err(fail("invalid Unicode escape sequence", &text));
        }
        self.i = j;
        let mut high: Option<u32> = None;
        for mut ch in codes {
            if (0xD800..0xDC00).contains(&ch) {
                if high.is_some() {
                    return Err(hard("22P02", high_high));
                }
                high = Some((ch & 0x3FF) << 10);
                continue;
            } else if (0xDC00..0xE000).contains(&ch) {
                let Some(h) = high.take() else { return Err(hard("22P02", no_high)) };
                ch = 0x10000 + h + (ch & 0x3FF);
            } else if high.is_some() {
                return Err(hard("22P02", no_high));
            }
            add_char(ch, s)?;
        }
        if high.is_some() {
            return Err(hard("22P02", no_high));
        }
        Ok(())
    }
}

/// Adds the character with code `ch` to `s`.
fn add_char(ch: u32, s: &mut String) -> Result<(), T> {
    if ch == 0 {
        return Err(hard("22P05", "unsupported Unicode escape sequence\u{1}\\u0000 cannot be converted to text."));
    }
    match char::from_u32(ch) {
        Some(c) => {
            s.push(c);
            Ok(())
        }
        None => Err(hard("42601", "invalid Unicode code point")),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kw {
    Is,
    To,
    Abs,
    Lax,
    Flag,
    Last,
    Null,
    Size,
    True,
    Type,
    With,
    False,
    Floor,
    Double,
    Exists,
    Starts,
    Strict,
    Ceiling,
    Unknown,
    Datetime,
    KeyValue,
    LikeRegex,
}

/// The keyword `w` is: they match in any case, except null, true and false.
fn keyword(w: &str) -> Option<Kw> {
    let lower = w.to_ascii_lowercase();
    let (kw, exact) = match lower.as_str() {
        "is" => (Kw::Is, false),
        "to" => (Kw::To, false),
        "abs" => (Kw::Abs, false),
        "lax" => (Kw::Lax, false),
        "flag" => (Kw::Flag, false),
        "last" => (Kw::Last, false),
        "null" => (Kw::Null, true),
        "size" => (Kw::Size, false),
        "true" => (Kw::True, true),
        "type" => (Kw::Type, false),
        "with" => (Kw::With, false),
        "false" => (Kw::False, true),
        "floor" => (Kw::Floor, false),
        "double" => (Kw::Double, false),
        "exists" => (Kw::Exists, false),
        "starts" => (Kw::Starts, false),
        "strict" => (Kw::Strict, false),
        "ceiling" => (Kw::Ceiling, false),
        "unknown" => (Kw::Unknown, false),
        "datetime" => (Kw::Datetime, false),
        "keyvalue" => (Kw::KeyValue, false),
        "like_regex" => (Kw::LikeRegex, false),
        _ => return None,
    };
    (!exact || lower == w).then_some(kw)
}

// ---------------------------------------------------------------------------------------------
// the grammar (jsonpath_gram.y)

/// What a stretch of path is: an expression or a predicate (what a filter wants).
#[derive(Clone, Copy, PartialEq, Eq)]
enum K {
    E,
    P,
}

/// Most items a path may have, and how deep the parser may nest: each parenthesis, bracket, filter
/// and sign counts two or so (Postgres's grammar stack is 10000 deep, its max_stack_depth stops the
/// walk at a few thousand).
const MAX_NODES: usize = 200_000;
const MAX_NEST: usize = 50_000;
/// The stack a parse or a walk may use, counted from where it began, on the thread it is called on
/// (a small one may be 2 MiB); past that it is done again on a thread with a stack of its own.
const INLINE_STACK: usize = 1 << 20;
/// The stack of that thread: only what a path needs of it is ever touched.
const BIG_STACKS: [usize; 3] = [1 << 30, 1 << 28, 1 << 26];

fn too_deep() -> PathError {
    ("54001", "stack depth limit exceeded".into())
}

/// What one walk may do: what it may add to its results, in `Value::weight` (Postgres would run out
/// of memory somewhere past there, for `$.**.**.**` and the like, or go on for hours: 2 GiB here) and
/// the steps it may take (it can be cancelled there; here nothing can).
const MAX_ITEMS: u64 = 1 << 25;
const MAX_STEPS: u64 = 1 << 31;

fn too_big(what: &str) -> PathError {
    ("54000", format!("the jsonpath query {what}"))
}

thread_local! {
    /// Set when a parse or a walk ran out of the stack it was given (rather than of nesting).
    static STACK_SHORT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether more than `budget` bytes of stack are in use since `base`: noted, if so.
fn stack_spent(base: usize, budget: usize) -> bool {
    let spent = super::stack_at().abs_diff(base) > budget;
    if spent {
        STACK_SHORT.with(|c| c.set(true));
    }
    spent
}

/// `f` with the stack it may use (a parse or a walk measures itself against it): on this thread if
/// it fits, else, run again, on a thread with as much stack as a path can need. A path
/// that is the answer for the same input every time, so running it twice costs only time.
fn with_stack<R: Send>(f: &(dyn Fn(usize) -> Result<R, PathError> + Sync)) -> Result<R, PathError> {
    STACK_SHORT.with(|c| c.set(false));
    let first = f(INLINE_STACK);
    if !STACK_SHORT.with(|c| c.get()) {
        return first;
    }
    for size in BIG_STACKS {
        let outcome = std::thread::scope(|scope| {
            let spawned = std::thread::Builder::new().stack_size(size).spawn_scoped(scope, || f(size - (1 << 20)));
            spawned.ok().map(|h| h.join())
        });
        match outcome {
            Some(Ok(r)) => return r,
            Some(Err(_)) => return Err(too_deep()),
            None => {} // no thread of that size: a smaller one
        }
    }
    Err(too_deep())
}

struct Parser<'t> {
    toks: &'t [Tok],
    i: usize,
    nodes: usize,
    /// where the stack stood when the parse began, and how much of it may be used
    stack: usize,
    budget: usize,
    depth: usize,
}

type PR<T> = Result<T, PathError>;

fn bin(a: Node, b: Node) -> (Box<Node>, Box<Node>) {
    (Box::new(a), Box::new(b))
}

impl Parser<'_> {
    fn node(&mut self, op: Op) -> PR<Node> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(too_deep());
        }
        Ok(Node { op, next: None })
    }

    /// The token ahead; a scanner failure is raised when the parser first reaches it.
    fn peek(&self) -> PR<&Tok> {
        let t = &self.toks[self.i];
        match &t.t {
            T::Fail(code, msg) => Err((code, msg.clone())),
            _ => Ok(t),
        }
    }

    fn peek2(&self) -> PR<&Tok> {
        let t = &self.toks[(self.i + 1).min(self.toks.len() - 1)];
        match &t.t {
            T::Fail(code, msg) => Err((code, msg.clone())),
            _ => Ok(t),
        }
    }

    fn bump(&mut self) {
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
    }

    fn unexpected<X>(&self) -> PR<X> {
        let t = self.peek()?;
        Err((
            "42601",
            if t.text.is_empty() {
                "syntax error at end of jsonpath input".to_string()
            } else {
                format!("syntax error at or near \"{}\" of jsonpath input", t.text)
            },
        ))
    }

    fn is_ch(&self, c: char) -> PR<bool> {
        Ok(self.peek()?.t == T::Ch(c))
    }

    fn is_kw(&self, k: Kw) -> PR<bool> {
        Ok(matches!(&self.peek()?.t, T::Word(w) if keyword(w) == Some(k)))
    }

    fn expect_ch(&mut self, c: char) -> PR<()> {
        if self.is_ch(c)? {
            self.bump();
            Ok(())
        } else {
            self.unexpected()
        }
    }

    /// One level deeper; Err if the nesting is too deep or the stack is getting used up.
    fn enter(&mut self) -> PR<()> {
        self.depth += 1;
        if self.depth > MAX_NEST || stack_spent(self.stack, self.budget) { Err(too_deep()) } else { Ok(()) }
    }

    fn path(&mut self, src: &str) -> PR<Path> {
        if self.peek()?.t == T::Eof {
            return Err(("22P02", format!("invalid input syntax for type jsonpath: \"{src}\"")));
        }
        let mut lax = true;
        if self.is_kw(Kw::Strict)? {
            lax = false;
            self.bump();
        } else if self.is_kw(Kw::Lax)? {
            self.bump();
        }
        let (expr, _) = self.or_level()?;
        if self.peek()?.t != T::Eof {
            return self.unexpected();
        }
        Ok(Path { lax, expr: Arc::new(expr), nodes: self.nodes })
    }

    /// `expr_or_predicate`, by precedence: `||` is the loosest.
    fn or_level(&mut self) -> PR<(Node, K)> {
        self.enter()?;
        let r = self.or_level_body();
        self.depth -= 1;
        r
    }

    fn or_level_body(&mut self) -> PR<(Node, K)> {
        let (mut l, mut k) = self.and_level()?;
        while self.peek()?.t == T::Or {
            if k != K::P {
                return self.unexpected();
            }
            self.bump();
            let (r, kr) = self.and_level()?;
            if kr != K::P {
                return self.unexpected();
            }
            let (a, b) = bin(l, r);
            l = self.node(Op::Or(a, b))?;
            k = K::P;
        }
        Ok((l, k))
    }

    fn and_level(&mut self) -> PR<(Node, K)> {
        let (mut l, mut k) = self.pred_primary()?;
        while self.peek()?.t == T::And {
            if k != K::P {
                return self.unexpected();
            }
            self.bump();
            let (r, kr) = self.pred_primary()?;
            if kr != K::P {
                return self.unexpected();
            }
            let (a, b) = bin(l, r);
            l = self.node(Op::And(a, b))?;
            k = K::P;
        }
        Ok((l, k))
    }

    fn predicate(&mut self) -> PR<Node> {
        let (n, k) = self.or_level()?;
        if k != K::P {
            return self.unexpected();
        }
        Ok(n)
    }

    /// `( predicate )` or `exists ( expr )`
    fn delimited(&mut self) -> PR<Node> {
        if self.is_ch('(')? {
            self.bump();
            let p = self.predicate()?;
            self.expect_ch(')')?;
            Ok(p)
        } else if self.is_kw(Kw::Exists)? {
            self.bump();
            self.expect_ch('(')?;
            let e = self.expr()?;
            self.expect_ch(')')?;
            self.node(Op::Exists(Box::new(e)))
        } else {
            self.unexpected()
        }
    }

    fn pred_primary(&mut self) -> PR<(Node, K)> {
        if self.peek()?.t == T::Not {
            self.bump();
            let p = self.delimited()?;
            return Ok((self.node(Op::Not(Box::new(p)))?, K::P));
        }
        if self.is_kw(Kw::Exists)? {
            return Ok((self.delimited()?, K::P));
        }
        if self.is_ch('(')? {
            return self.paren();
        }
        let e = self.add_expr(None)?;
        self.after_expr(e)
    }

    /// A predicate or expression that starts with `(`.
    fn paren(&mut self) -> PR<(Node, K)> {
        self.bump();
        let (inner, k) = self.or_level()?;
        self.expect_ch(')')?;
        if k == K::P {
            if self.is_kw(Kw::Is)? {
                self.bump();
                if !self.is_kw(Kw::Unknown)? {
                    return self.unexpected();
                }
                self.bump();
                return Ok((self.node(Op::IsUnknown(Box::new(inner)))?, K::P));
            }
            if !self.accessor_ahead()? {
                return Ok((inner, K::P));
            }
        }
        let mut head = inner;
        self.accessors(&mut head)?;
        let e = self.add_expr(Some(head))?;
        self.after_expr(e)
    }

    fn accessor_ahead(&self) -> PR<bool> {
        Ok(matches!(self.peek()?.t, T::Ch('.' | '[' | '?')))
    }

    /// An expression just read: a comparison, `starts with` or `like_regex` makes a predicate.
    fn after_expr(&mut self, e: Node) -> PR<(Node, K)> {
        let cmp = match self.peek()?.t {
            T::Eq => Some(Cmp::Eq),
            T::Ne => Some(Cmp::Ne),
            T::Lt => Some(Cmp::Lt),
            T::Le => Some(Cmp::Le),
            T::Gt => Some(Cmp::Gt),
            T::Ge => Some(Cmp::Ge),
            _ => None,
        };
        if let Some(c) = cmp {
            self.bump();
            let r = self.expr()?;
            let (a, b) = bin(e, r);
            return Ok((self.node(Op::Cmp(c, a, b))?, K::P));
        }
        if self.is_kw(Kw::Starts)? {
            self.bump();
            if !self.is_kw(Kw::With)? {
                return self.unexpected();
            }
            self.bump();
            let initial = match self.peek()?.t.clone() {
                T::Str(s) => Op::Str(s),
                T::Var(v) => Op::Var(v),
                _ => return self.unexpected(),
            };
            self.bump();
            let initial = self.node(initial)?;
            let (a, b) = bin(e, initial);
            return Ok((self.node(Op::StartsWith(a, b))?, K::P));
        }
        if self.is_kw(Kw::LikeRegex)? {
            self.bump();
            let T::Str(pattern) = self.peek()?.t.clone() else { return self.unexpected() };
            self.bump();
            let mut flags = None;
            if self.is_kw(Kw::Flag)? {
                self.bump();
                let T::Str(f) = self.peek()?.t.clone() else { return self.unexpected() };
                self.bump();
                flags = Some(f);
            } else {
                self.peek()?; // the token ahead is read before the predicate is made
            }
            let n = self.like_regex(e, pattern, flags.as_deref())?;
            return Ok((n, K::P));
        }
        Ok((e, K::E))
    }

    fn like_regex(&mut self, e: Node, pattern: String, flags: Option<&str>) -> PR<Node> {
        let mut bits = 0u8;
        for c in flags.unwrap_or("").chars() {
            bits |= match c {
                'i' => RE_I,
                's' => RE_S,
                'm' => RE_M,
                'x' => RE_X,
                'q' => RE_Q,
                // (what follows `\u{1}` is the DETAIL: see `path_error`)
                _ => {
                    return Err((
                        "42601",
                        format!(
                            "invalid input syntax for type jsonpath\u{1}Unrecognized flag character \"{c}\" in LIKE_REGEX predicate."
                        ),
                    ));
                }
            };
        }
        if bits & RE_X != 0 && bits & RE_Q == 0 {
            return Err(("0A000", "XQuery \"x\" flag (expanded regular expressions) is not implemented".into()));
        }
        let flags = are::Flags {
            icase: bits & RE_I != 0,
            dotall: bits & RE_S != 0,
            multiline: bits & RE_M != 0,
            quote: bits & RE_Q != 0,
        };
        let re = are::Regex::new(&pattern, flags, (self.stack, self.budget))
            .map_err(|msg| ("2201B", format!("invalid regular expression: {msg}")))?;
        self.node(Op::LikeRegex(Box::new(e), pattern, bits, Arc::new(re)))
    }

    fn expr(&mut self) -> PR<Node> {
        self.enter()?;
        let r = self.add_expr(None);
        self.depth -= 1;
        r
    }

    fn add_expr(&mut self, first: Option<Node>) -> PR<Node> {
        let mut l = self.mul_expr(first)?;
        loop {
            let op = match self.peek()?.t {
                T::Ch('+') => Ar::Add,
                T::Ch('-') => Ar::Sub,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.mul_expr(None)?;
            let (a, b) = bin(l, r);
            l = self.node(Op::Bin(op, a, b))?;
        }
    }

    fn mul_expr(&mut self, first: Option<Node>) -> PR<Node> {
        let mut l = self.unary(first)?;
        loop {
            let op = match self.peek()?.t {
                T::Ch('*') => Ar::Mul,
                T::Ch('/') => Ar::Div,
                T::Ch('%') => Ar::Mod,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.unary(None)?;
            let (a, b) = bin(l, r);
            l = self.node(Op::Bin(op, a, b))?;
        }
    }

    fn unary(&mut self, first: Option<Node>) -> PR<Node> {
        if let Some(n) = first {
            return Ok(n);
        }
        self.enter()?;
        let r = self.unary_body();
        self.depth -= 1;
        r
    }

    fn unary_body(&mut self) -> PR<Node> {
        let plus = match self.peek()?.t {
            T::Ch('+') => true,
            T::Ch('-') => false,
            _ => return self.accessor_expr(),
        };
        self.bump();
        let mut arg = self.unary(None)?;
        // a sign on a plain number is part of the number, as the grammar makes it
        if arg.next.is_none()
            && let Op::Num(d) = &arg.op
        {
            arg.op = Op::Num(if plus { d.clone() } else { d.neg() });
            return Ok(arg);
        }
        let arg = Box::new(arg);
        self.node(if plus { Op::Plus(arg) } else { Op::Minus(arg) })
    }

    fn accessor_expr(&mut self) -> PR<Node> {
        let mut head = self.primary()?;
        self.accessors(&mut head)?;
        Ok(head)
    }

    fn primary(&mut self) -> PR<Node> {
        let op = match self.peek()?.t.clone() {
            T::Str(s) => Op::Str(s),
            T::Var(v) => Op::Var(v),
            T::Num(n) => Op::Num(literal(&n)?),
            T::Word(w) => match keyword(&w) {
                Some(Kw::Null) => Op::Null,
                Some(Kw::True) => Op::Bool(true),
                Some(Kw::False) => Op::Bool(false),
                Some(Kw::Last) => Op::Last,
                _ => return self.unexpected(),
            },
            T::Ch('$') => Op::Root,
            T::Ch('@') => Op::Current,
            T::Ch('(') => {
                self.bump();
                let (n, k) = self.or_level()?;
                self.expect_ch(')')?;
                if k == K::P && !self.accessor_ahead()? {
                    return self.unexpected();
                }
                return Ok(n);
            }
            _ => return self.unexpected(),
        };
        self.bump();
        self.node(op)
    }

    /// Accessors after `head`, each added to the end of its chain.
    fn accessors(&mut self, head: &mut Node) -> PR<()> {
        // the end of the chain so far, found once
        let mut tail = head;
        while tail.next.is_some() {
            tail = tail.next.as_deref_mut().unwrap_or_else(|| unreachable!("there is a next"));
        }
        loop {
            let n = match self.peek()?.t {
                T::Ch('.') => {
                    self.bump();
                    self.dot()?
                }
                T::Ch('[') => {
                    self.bump();
                    self.subscript()?
                }
                T::Ch('?') => {
                    self.bump();
                    self.expect_ch('(')?;
                    let p = self.predicate()?;
                    self.expect_ch(')')?;
                    self.node(Op::Filter(Box::new(p)))?
                }
                _ => return Ok(()),
            };
            tail.next = Some(Box::new(n));
            tail = tail.next.as_deref_mut().unwrap_or_else(|| unreachable!("it was just set"));
        }
    }

    /// What follows a `.`: a key, `*`, `**` or an item method.
    fn dot(&mut self) -> PR<Node> {
        match self.peek()?.t.clone() {
            T::Str(s) => {
                self.bump();
                self.node(Op::Key(s))
            }
            T::Ch('*') => {
                self.bump();
                self.node(Op::AnyKey)
            }
            T::Any => {
                self.bump();
                self.any_path()
            }
            T::Word(w) => {
                let kw = keyword(&w);
                let method = match kw {
                    Some(Kw::Type) => Some(Method::Type),
                    Some(Kw::Size) => Some(Method::Size),
                    Some(Kw::Abs) => Some(Method::Abs),
                    Some(Kw::Floor) => Some(Method::Floor),
                    Some(Kw::Ceiling) => Some(Method::Ceiling),
                    Some(Kw::Double) => Some(Method::Double),
                    Some(Kw::KeyValue) => Some(Method::KeyValue),
                    _ => None,
                };
                let callable = method.is_some() || kw == Some(Kw::Datetime);
                if callable && self.peek2()?.t == T::Ch('(') {
                    self.bump();
                    self.bump();
                    let op = match method {
                        Some(m) => Op::Method(m),
                        None => {
                            let template = match self.peek()?.t.clone() {
                                T::Str(s) => {
                                    self.bump();
                                    Some(s)
                                }
                                _ => None,
                            };
                            Op::Datetime(template)
                        }
                    };
                    self.expect_ch(')')?;
                    return self.node(op);
                }
                self.bump();
                self.node(Op::Key(w))
            }
            _ => self.unexpected(),
        }
    }

    /// After `**`: an optional `{n}`, `{n to m}`, with `last` for n and m.
    fn any_path(&mut self) -> PR<Node> {
        if !self.is_ch('{')? {
            return self.node(Op::Any(0, u32::MAX));
        }
        self.bump();
        let first = self.any_level()?;
        let last = if self.is_kw(Kw::To)? {
            self.bump();
            self.any_level()?
        } else {
            first
        };
        self.expect_ch('}')?;
        self.node(Op::Any(first, last))
    }

    fn any_level(&mut self) -> PR<u32> {
        match self.peek()?.t.clone() {
            T::Num(n) if n.int => {
                let shown = self.peek()?.text.clone();
                let v = u128::from_str_radix(&n.text, n.radix).ok().and_then(|v| i32::try_from(v).ok());
                let Some(v) = v else {
                    return Err(("22003", format!("value \"{shown}\" is out of range for type integer")));
                };
                self.bump();
                Ok(v as u32)
            }
            T::Word(w) if keyword(&w) == Some(Kw::Last) => {
                self.bump();
                Ok(u32::MAX)
            }
            _ => self.unexpected(),
        }
    }

    /// After `[`: `*]` or `expr [to expr] {, ...}]`.
    fn subscript(&mut self) -> PR<Node> {
        if self.is_ch('*')? {
            self.bump();
            self.expect_ch(']')?;
            return self.node(Op::AnyArray);
        }
        let mut elems = vec![];
        loop {
            let from = self.expr()?;
            let to = if self.is_kw(Kw::To)? {
                self.bump();
                Some(self.expr()?)
            } else {
                None
            };
            elems.push((from, to));
            if self.is_ch(',')? {
                self.bump();
            } else if self.is_ch(']')? {
                self.bump();
                return self.node(Op::Index(elems));
            } else {
                return self.unexpected();
            }
        }
    }
}

/// What Postgres checks once a path has parsed: `@` only inside a filter, `last` only inside
/// an array subscript.
fn check(n: &Node, nest: u32, sub: bool, stack: (usize, usize)) -> Result<(), PathError> {
    if stack_spent(stack.0, stack.1) {
        return Err(too_deep());
    }
    let mut cur = Some(n);
    while let Some(n) = cur {
        match &n.op {
            Op::Bin(_, a, b) | Op::Cmp(_, a, b) | Op::And(a, b) | Op::Or(a, b) | Op::StartsWith(a, b) => {
                check(a, nest, sub, stack)?;
                check(b, nest, sub, stack)?;
            }
            Op::LikeRegex(a, ..) | Op::Not(a) | Op::IsUnknown(a) | Op::Plus(a) | Op::Minus(a) | Op::Exists(a) => {
                check(a, nest, sub, stack)?
            }
            Op::Filter(a) => check(a, nest + 1, sub, stack)?,
            Op::Current if nest == 0 => {
                return Err(("42601", "@ is not allowed in root expressions".into()));
            }
            Op::Last if !sub => {
                return Err(("42601", "LAST is allowed only in array subscripts".into()));
            }
            Op::Index(elems) => {
                for (from, to) in elems {
                    check(from, nest, true, stack)?;
                    if let Some(to) = to {
                        check(to, nest, true, stack)?;
                    }
                }
            }
            _ => {}
        }
        cur = n.next.as_deref();
    }
    Ok(())
}

/// Reads a path, as `'...'::jsonpath` does; the error has the SQLSTATE that goes with it (42601
/// for a syntax error, but also 2201B for a `like_regex` pattern that isn't a regular
/// expression, 22P05 for `\u0000`, ...).
pub fn parse_code(src: &str) -> Result<Path, PathError> {
    let toks = Lexer::all(src);
    with_stack(&|budget| {
        let mut p = Parser { toks: &toks, i: 0, nodes: 0, stack: super::stack_at(), budget, depth: 0 };
        let path = p.path(src)?;
        check(&path.expr, 0, false, (p.stack, budget))?;
        Ok(path)
    })
}

/// Reads a path; the error is Postgres's message (the caller adds SQLSTATE 42601, or takes it
/// from `parse_code` when a message isn't a syntax error).
pub fn parse(src: &str) -> Result<Path, String> {
    parse_code(src).map_err(|e| e.1)
}

// ---------------------------------------------------------------------------------------------
// jsonb's layout, for .keyvalue()'s id

fn is_container(j: &Value) -> bool {
    j.is_container()
}

/// Where the array or object `target`, somewhere in `base`, starts in `base`.
fn container_offset(base: &Value, target: &Value) -> usize {
    let mut hit = 0;
    value::lay(base, 0, &mut false, &mut |v, at| {
        let found = std::ptr::eq(v, target);
        if found {
            hit = at;
        }
        found
    });
    hit
}

/// Where every array and object in `base` starts, by address.
fn all_offsets(base: &Value) -> HashMap<usize, usize> {
    let mut all = HashMap::new();
    value::lay(base, 0, &mut false, &mut |v, at| {
        all.insert(v as *const Value as usize, at);
        false
    });
    all
}

// ---------------------------------------------------------------------------------------------
// the walk (jsonpath_exec.c)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Res {
    Ok,
    NotFound,
    Error,
}

/// A predicate's answer: true, false or unknown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tri {
    True,
    False,
    Unknown,
}

type R = Result<Res, PathError>;
/// Where the items found go; None when only whether there is one matters.
type F<'f> = Option<&'f mut Vec<Value>>;

/// Where `@`, and the object `.keyvalue()` ids count from, are.
#[derive(Clone, Copy)]
struct Env<'e> {
    cur: &'e Value,
    base: &'e Value,
    id: i64,
}

struct Exec<'a> {
    lax: bool,
    /// lax mode, and inside `.**`: a missing key or a wrong type is no error but nothing found
    ignore_structural: bool,
    /// errors are raised; else they are just a `Res::Error` (what a filter and `silent` want)
    throw: bool,
    root: &'a Value,
    vars: &'a Value,
    /// the size of the array whose subscript is being read, for `last`
    last_size: i64,
    /// the base id of the next object a method makes
    next_id: i64,
    /// where the containers of the target and of the variables start (see `lay`), once needed
    offsets: [Option<HashMap<usize, usize>>; 2],
    /// where the stack stood when the walk began, and how much of it may be used
    stack: usize,
    budget: usize,
    /// the `_tz` functions: a date or time may be compared with a timestamp with a time zone
    tz: bool,
    /// what the walk has done so far, against `MAX_STEPS` and `MAX_ITEMS`
    steps: std::cell::Cell<u64>,
    items: std::cell::Cell<u64>,
    max_items: u64,
}

fn type_name(j: &Value) -> &'static str {
    match j {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Datetime(d) => d.type_name(),
    }
}

/// `strtod`'s reading of a string, as float8in uses it: blanks around, a decimal or hex float.
/// None if it is not one, or overflows or underflows to zero (a denormal is fine).
fn float8(s: &str) -> Option<f64> {
    let s = s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'));
    let (neg, body) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if body.starts_with(['+', '-']) {
        return None;
    }
    let v = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        let (mant, exp) = match h.find(['p', 'P']) {
            Some(k) => (&h[..k], h[k + 1..].parse::<i32>().ok()?),
            None => (h, 0),
        };
        let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
        if int.is_empty() && frac.is_empty() || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let digits: f64 =
            int.chars().chain(frac.chars()).fold(0.0, |a, c| a * 16.0 + f64::from(c.to_digit(16).unwrap()));
        digits * 2f64.powi(exp - 4 * frac.len() as i32)
    } else {
        let ok = body.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'));
        if !ok || !body.bytes().any(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: f64 = body.parse().ok()?;
        let nonzero = body.split(['e', 'E']).next()?.bytes().any(|b| matches!(b, b'1'..=b'9'));
        if v == 0.0 && nonzero {
            return None;
        }
        v
    };
    (v.is_finite()).then_some(if neg { -v } else { v })
}

/// Whether a number can be a float8 (what `.double()` of a number checks): not past its range, and not
/// so small it is zero.
fn in_float8_range(n: &Numeric) -> bool {
    let f = n.to_f64();
    f.is_finite() && (f != 0.0 || n.is_zero())
}

/// How two scalars compare under `op`, as jsonpath's comparison does. Err is a comparison that
/// raises whatever the mode (a date against a timestamp with a time zone, without `_tz`).
fn compare(op: Cmp, a: &Value, b: &Value, tz: bool) -> Result<Tri, PathError> {
    let rank = |j: &Value| match j {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Datetime(_) => 4,
        _ => 5,
    };
    if rank(a) != rank(b) {
        if a.is_null() || b.is_null() {
            // null is not equal to, and not ordered against, anything but null
            return Ok(if op == Cmp::Ne { Tri::True } else { Tri::False });
        }
        return Ok(Tri::Unknown);
    }
    let ord = match (a, b) {
        (Value::Null, _) => Ordering::Equal,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Number(x), Value::Number(y)) => x.cmp(y),
        (Value::String(x), Value::String(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Datetime(x), Value::Datetime(y)) => match datetime::compare(x, y, tz)? {
            Some(o) => o,
            None => return Ok(Tri::Unknown), // a date and a time, say
        },
        _ => return Ok(Tri::Unknown), // arrays and objects don't compare
    };
    let t = match op {
        Cmp::Eq => ord.is_eq(),
        Cmp::Ne => ord.is_ne(),
        Cmp::Lt => ord.is_lt(),
        Cmp::Le => ord.is_le(),
        Cmp::Gt => ord.is_gt(),
        Cmp::Ge => ord.is_ge(),
    };
    Ok(if t { Tri::True } else { Tri::False })
}

fn ar_name(op: Ar) -> &'static str {
    match op {
        Ar::Add => "+",
        Ar::Sub => "-",
        Ar::Mul => "*",
        Ar::Div => "/",
        Ar::Mod => "%",
    }
}

fn method_name(m: Method) -> &'static str {
    match m {
        Method::Type => "type",
        Method::Size => "size",
        Method::Abs => "abs",
        Method::Floor => "floor",
        Method::Ceiling => "ceiling",
        Method::Double => "double",
        Method::KeyValue => "keyvalue",
    }
}

fn arith(op: Ar, a: &Numeric, b: &Numeric) -> Result<Numeric, PathError> {
    match op {
        Ar::Add => a.add(b),
        Ar::Sub => a.sub(b),
        Ar::Mul => a.mul(b),
        Ar::Div => a.div(b),
        Ar::Mod => a.rem(b),
    }
    .map_err(NumError::err)
}

impl Exec<'_> {
    /// An error in the path: raised, or (inside a predicate, or `silent`) just noted.
    fn fail(&self, code: &'static str, msg: impl Into<String>) -> R {
        if self.throw { Err((code, msg.into())) } else { Ok(Res::Error) }
    }

    fn deep(&self) -> Result<(), PathError> {
        let steps = self.steps.get() + 1;
        self.steps.set(steps);
        if steps > MAX_STEPS {
            Err(too_big("takes too many steps"))
        } else if stack_spent(self.stack, self.budget) {
            Err(too_deep())
        } else {
            Ok(())
        }
    }

    /// `v` into what the walk found, if that is not too much.
    fn emit(&self, found: &mut Vec<Value>, v: Value) -> Result<(), PathError> {
        let items = self.items.get() + v.weight();
        self.items.set(items);
        if items > self.max_items {
            return Err(too_big("finds too many items"));
        }
        found.push(v);
        Ok(())
    }

    /// `node` against `jb`, in lax mode unwrapping an array it can't apply to.
    fn item(&mut self, env: Env, node: &Node, jb: &Value, found: F) -> R {
        self.unwrap_item(env, node, jb, found, self.lax)
    }

    /// `node`'s next item against `v`, or `v` into `found` if there is none.
    fn next(&mut self, env: Env, node: &Node, v: &Value, mut found: F) -> R {
        match &node.next {
            Some(n) => self.item(env, n, v, found.as_deref_mut()),
            None => {
                if let Some(f) = found {
                    self.emit(f, v.clone())?;
                }
                Ok(Res::Ok)
            }
        }
    }

    /// `node` against `jb`, then (lax mode) an array among its results unwrapped one level.
    fn item_unwrapped(&mut self, env: Env, node: &Node, jb: &Value, unwrap: bool, mut found: F) -> R {
        if unwrap && self.lax {
            let mut seq = vec![];
            let res = self.item(env, node, jb, Some(&mut seq))?;
            if res == Res::Error {
                return Ok(res);
            }
            for v in seq {
                if v.is_array() {
                    self.any(env, None, &v, found.as_deref_mut(), 1, 1, 1, false, false)?;
                } else if let Some(f) = found.as_deref_mut() {
                    self.emit(f, v)?;
                }
            }
            return Ok(Res::Ok);
        }
        self.item(env, node, jb, found)
    }

    /// The same with errors suppressed: what a predicate's operands use.
    fn item_quiet(&mut self, env: Env, node: &Node, jb: &Value, unwrap: bool, found: F) -> R {
        let saved = self.throw;
        self.throw = false;
        let r = self.item_unwrapped(env, node, jb, unwrap, found);
        self.throw = saved;
        r
    }

    /// `node` (the item after an array accessor, or the accessor itself when it unwraps)
    /// against each element of the array `jb`.
    fn elements(&mut self, env: Env, node: Option<&Node>, jb: &Value, found: F, unwrap: bool) -> R {
        self.any(env, node, jb, found, 1, 1, 1, false, unwrap)
    }

    /// The members of `jb` (an array, or an object's values) at depths `first` to `last`, each
    /// given to `node`, or added to `found` if there's no node.
    #[allow(clippy::too_many_arguments)]
    fn any(
        &mut self,
        env: Env,
        node: Option<&Node>,
        jb: &Value,
        mut found: F,
        level: u32,
        first: u32,
        last: u32,
        ignore: bool,
        unwrap: bool,
    ) -> R {
        let mut res = Res::NotFound;
        if level > last {
            return Ok(res);
        }
        self.deep()?;
        let members: Vec<&Value> = match jb {
            Value::Array(a) => a.iter().collect(),
            Value::Object(o) => o.values().collect(),
            _ => vec![],
        };
        for v in members {
            if level >= first || (first == u32::MAX && last == u32::MAX && !is_container(v)) {
                if let Some(node) = node {
                    if ignore {
                        let saved = self.ignore_structural;
                        self.ignore_structural = true;
                        let r = self.unwrap_item(env, node, v, found.as_deref_mut(), unwrap);
                        self.ignore_structural = saved;
                        res = r?;
                    } else {
                        res = self.unwrap_item(env, node, v, found.as_deref_mut(), unwrap)?;
                    }
                    if res == Res::Error || (res == Res::Ok && found.is_none()) {
                        break;
                    }
                } else if let Some(f) = found.as_deref_mut() {
                    self.emit(f, v.clone())?;
                } else {
                    return Ok(Res::Ok);
                }
            }
            if level < last && is_container(v) {
                res = self.any(env, node, v, found.as_deref_mut(), level + 1, first, last, ignore, unwrap)?;
                if res == Res::Error || (res == Res::Ok && found.is_none()) {
                    break;
                }
            }
        }
        Ok(res)
    }

    fn unwrap_item(&mut self, env: Env, node: &Node, jb: &Value, mut found: F, unwrap: bool) -> R {
        self.deep()?;
        match &node.op {
            Op::And(..)
            | Op::Or(..)
            | Op::Not(_)
            | Op::IsUnknown(_)
            | Op::Cmp(..)
            | Op::Exists(_)
            | Op::StartsWith(..)
            | Op::LikeRegex(..) => {
                let st = self.boolean(env, node, jb)?;
                if node.next.is_none() && found.is_none() {
                    return Ok(Res::Ok);
                }
                let v = match st {
                    Tri::Unknown => Value::Null,
                    t => Value::Bool(t == Tri::True),
                };
                self.next(env, node, &v, found)
            }
            Op::Key(name) => match jb {
                Value::Object(o) => match o.get(name) {
                    Some(v) => self.next(env, node, v, found),
                    None if !self.ignore_structural => {
                        self.fail("2203A", format!("JSON object does not contain key \"{name}\""))
                    }
                    None => Ok(Res::NotFound),
                },
                Value::Array(_) if unwrap => self.elements(env, Some(node), jb, found, false),
                _ if !self.ignore_structural => {
                    self.fail("2203A", "jsonpath member accessor can only be applied to an object")
                }
                _ => Ok(Res::NotFound),
            },
            Op::Root => {
                let env = Env { base: self.root, id: 0, ..env };
                self.next(env, node, self.root, found)
            }
            Op::Current => self.next(env, node, env.cur, found),
            Op::AnyArray => {
                if jb.is_array() {
                    self.elements(env, node.next.as_deref(), jb, found, self.lax)
                } else if self.lax {
                    self.next(env, node, jb, found)
                } else if !self.ignore_structural {
                    self.fail("22039", "jsonpath wildcard array accessor can only be applied to an array")
                } else {
                    Ok(Res::NotFound)
                }
            }
            Op::Index(subs) => {
                if !(jb.is_array() || self.lax) {
                    if !self.ignore_structural {
                        return self.fail("22039", "jsonpath array accessor can only be applied to an array");
                    }
                    return Ok(Res::NotFound);
                }
                let saved = self.last_size;
                let (size, single) = match jb {
                    Value::Array(a) => (a.len() as i64, false),
                    _ => (1, true),
                };
                self.last_size = size;
                let has_next = node.next.is_some();
                let mut res = Res::NotFound;
                'subs: for (from, to) in subs {
                    let r = self.array_index(env, from, jb)?;
                    let Ok(index_from) = r else {
                        res = Res::Error;
                        break;
                    };
                    let index_to = match to {
                        Some(to) => match self.array_index(env, to, jb)? {
                            Ok(i) => i,
                            Err(()) => {
                                res = Res::Error;
                                break;
                            }
                        },
                        None => index_from,
                    };
                    if !self.ignore_structural && (index_from < 0 || index_from > index_to || index_to >= size) {
                        return self.fail("22033", "jsonpath array subscript is out of bounds");
                    }
                    let (from, to) = (index_from.max(0), index_to.min(size - 1));
                    res = Res::NotFound;
                    for i in from..=to {
                        let v = if single {
                            jb
                        } else {
                            match jb {
                                Value::Array(a) => match a.get(i as usize) {
                                    Some(v) => v,
                                    None => continue,
                                },
                                _ => continue,
                            }
                        };
                        if !has_next && found.is_none() {
                            return Ok(Res::Ok);
                        }
                        res = self.next(env, node, v, found.as_deref_mut())?;
                        if res == Res::Error || (res == Res::Ok && found.is_none()) {
                            break 'subs;
                        }
                    }
                }
                self.last_size = saved;
                Ok(res)
            }
            Op::Last => {
                if self.last_size < 0 {
                    return Err(("XX000", "evaluating jsonpath LAST outside of array subscript".into()));
                }
                if node.next.is_none() && found.is_none() {
                    return Ok(Res::Ok);
                }
                let last = Value::from_i64(self.last_size - 1);
                self.next(env, node, &last, found)
            }
            Op::AnyKey => match jb {
                Value::Object(_) => self.any(env, node.next.as_deref(), jb, found, 1, 1, 1, false, self.lax),
                Value::Array(_) if unwrap => self.elements(env, Some(node), jb, found, false),
                _ if !self.ignore_structural => {
                    self.fail("2203C", "jsonpath wildcard member accessor can only be applied to an object")
                }
                _ => Ok(Res::NotFound),
            },
            Op::Bin(..) => self.binary(env, node, jb, found),
            Op::Plus(a) => self.unary(env, node, false, a, jb, found),
            Op::Minus(a) => self.unary(env, node, true, a, jb, found),
            Op::Filter(pred) => {
                if unwrap && jb.is_array() {
                    return self.elements(env, Some(node), jb, found, false);
                }
                let st = self.boolean(Env { cur: jb, ..env }, pred, jb)?;
                if st != Tri::True { Ok(Res::NotFound) } else { self.next(env, node, jb, found) }
            }
            Op::Any(first, last) => {
                let mut res = Res::NotFound;
                if *first == 0 {
                    let saved = self.ignore_structural;
                    self.ignore_structural = true;
                    let r = self.next(env, node, jb, found.as_deref_mut());
                    self.ignore_structural = saved;
                    res = r?;
                    if res == Res::Ok && found.is_none() {
                        return Ok(res);
                    }
                }
                if is_container(jb) {
                    res = self.any(env, node.next.as_deref(), jb, found, 1, *first, *last, true, self.lax)?;
                }
                Ok(res)
            }
            Op::Null | Op::Bool(_) | Op::Num(_) | Op::Str(_) => {
                if node.next.is_none() && found.is_none() {
                    return Ok(Res::Ok);
                }
                let v = match &node.op {
                    Op::Bool(b) => Value::Bool(*b),
                    Op::Num(d) => Value::Number(d.clone()),
                    Op::Str(s) => Value::String(s.clone()),
                    _ => Value::Null,
                };
                self.next(env, node, &v, found)
            }
            Op::Var(name) => {
                let Value::Object(vars) = self.vars else { unreachable!("the variables are an object") };
                let Some(v) = vars.get(name) else {
                    return Err(("42704", format!("could not find jsonpath variable \"{name}\"")));
                };
                let env = Env { base: self.vars, id: 1, ..env };
                self.next(env, node, v, found)
            }
            Op::Method(m) => self.method(env, node, *m, jb, unwrap, found),
            Op::Datetime(template) => self.datetime(env, node, template.as_deref(), jb, unwrap, found),
        }
    }

    /// `.datetime()` and `.datetime(template)`: a string read as a date, time or timestamp.
    fn datetime(&mut self, env: Env, node: &Node, template: Option<&str>, jb: &Value, unwrap: bool, found: F) -> R {
        if unwrap && jb.is_array() {
            return self.elements(env, Some(node), jb, found, false);
        }
        let Value::String(s) = jb else {
            return self.fail("22031", "jsonpath item method .datetime() can only be applied to a string");
        };
        let dt = match template {
            Some(t) => match datetime::parse_template(s, t) {
                Ok(d) => d,
                Err(e) => match e.parts() {
                    // a template that is none is an error whatever `silent` says
                    (true, err) => return Err(err),
                    (false, (code, msg)) => return self.fail(code, msg),
                },
            },
            None => match datetime::parse_iso(s) {
                Some(d) => d,
                // (what follows `\u{2}` is the HINT: see `path_error`)
                None => {
                    return self.fail(
                        "22031",
                        format!(
                            "datetime format is not recognized: \"{s}\"\u{2}Use a datetime template argument to \
                             specify the input data format."
                        ),
                    );
                }
            },
        };
        if node.next.is_none() && found.is_none() {
            return Ok(Res::Ok);
        }
        self.next(env, node, &Value::Datetime(dt), found)
    }

    /// A subscript's value: its integer part, or Err(()) if it had an error noted.
    fn array_index(&mut self, env: Env, node: &Node, jb: &Value) -> Result<Result<i64, ()>, PathError> {
        let mut found = vec![];
        if self.item(env, node, jb, Some(&mut found))? == Res::Error {
            return Ok(Err(()));
        }
        let (code, msg) = match found.as_slice() {
            [Value::Number(n)] => match n.trunc_i32() {
                Some(i) => return Ok(Ok(i64::from(i))),
                None => ("22033", "jsonpath array subscript is out of integer range"),
            },
            _ => ("22033", "jsonpath array subscript is not a single numeric value"),
        };
        self.fail(code, msg)?;
        Ok(Err(()))
    }

    fn binary(&mut self, env: Env, node: &Node, jb: &Value, found: F) -> R {
        let Op::Bin(op, l, r) = &node.op else { unreachable!("binary is for arithmetic") };
        let op = *op;
        let (mut lseq, mut rseq) = (vec![], vec![]);
        if self.item_unwrapped(env, l, jb, true, Some(&mut lseq))? == Res::Error {
            return Ok(Res::Error);
        }
        if self.item_unwrapped(env, r, jb, true, Some(&mut rseq))? == Res::Error {
            return Ok(Res::Error);
        }
        let num = |seq: &[Value]| match seq {
            [Value::Number(n)] => Some(n.clone()),
            _ => None,
        };
        let Some(x) = num(&lseq) else {
            return self.fail(
                "22038",
                format!("left operand of jsonpath operator {} is not a single numeric value", ar_name(op)),
            );
        };
        let Some(y) = num(&rseq) else {
            return self.fail(
                "22038",
                format!("right operand of jsonpath operator {} is not a single numeric value", ar_name(op)),
            );
        };
        let v = match arith(op, &x, &y) {
            Ok(v) => Value::Number(v),
            Err((code, msg)) => return self.fail(code, msg),
        };
        if node.next.is_none() && found.is_none() {
            return Ok(Res::Ok);
        }
        self.next(env, node, &v, found)
    }

    fn unary(&mut self, env: Env, node: &Node, neg: bool, arg: &Node, jb: &Value, mut found: F) -> R {
        let mut seq = vec![];
        if self.item_unwrapped(env, arg, jb, true, Some(&mut seq))? == Res::Error {
            return Ok(Res::Error);
        }
        let has_next = node.next.is_some();
        let mut res = Res::NotFound;
        for v in seq {
            let Value::Number(n) = &v else {
                if found.is_none() && !has_next {
                    continue;
                }
                return self.fail(
                    "2203B",
                    format!(
                        "operand of unary jsonpath operator {} is not a numeric value",
                        if neg { '-' } else { '+' }
                    ),
                );
            };
            if found.is_none() && !has_next {
                return Ok(Res::Ok);
            }
            let out = if neg { Value::Number(n.neg()) } else { v.clone() };
            let r = self.next(env, node, &out, found.as_deref_mut())?;
            if r == Res::Error {
                return Ok(r);
            }
            if r == Res::Ok {
                if found.is_none() {
                    return Ok(r);
                }
                res = Res::Ok;
            }
        }
        Ok(res)
    }

    fn method(&mut self, env: Env, node: &Node, m: Method, jb: &Value, unwrap: bool, found: F) -> R {
        let name = method_name(m);
        match m {
            Method::Type => {
                let v = Value::String(type_name(jb).into());
                self.next(env, node, &v, found)
            }
            Method::Size => {
                let size = match jb {
                    Value::Array(a) => a.len(),
                    _ if self.lax => 1,
                    _ if !self.ignore_structural => {
                        return self.fail("22039", "jsonpath item method .size() can only be applied to an array");
                    }
                    _ => return Ok(Res::NotFound),
                };
                self.next(env, node, &Value::from_usize(size), found)
            }
            Method::Abs | Method::Floor | Method::Ceiling => {
                if unwrap && jb.is_array() {
                    return self.elements(env, Some(node), jb, found, false);
                }
                let Value::Number(n) = jb else {
                    return self.fail(
                        "22036",
                        format!("jsonpath item method .{name}() can only be applied to a numeric value"),
                    );
                };
                let d = match m {
                    Method::Abs => n.abs(),
                    Method::Floor => n.floor(),
                    _ => n.ceil(),
                };
                if node.next.is_none() && found.is_none() {
                    return Ok(Res::Ok);
                }
                self.next(env, node, &Value::Number(d), found)
            }
            Method::Double => {
                if unwrap && jb.is_array() {
                    return self.elements(env, Some(node), jb, found, false);
                }
                match jb {
                    Value::Number(n) => {
                        // a number that is a double is passed on as it is, digits and all
                        if !in_float8_range(n) {
                            return self.fail(
                                "22036",
                                "numeric argument of jsonpath item method .double() is out of range for type double precision",
                            );
                        }
                        self.next(env, node, jb, found)
                    }
                    Value::String(s) => {
                        let Some(f) = float8(s) else {
                            return self.fail(
                                "22036",
                                "string argument of jsonpath item method .double() is not a valid representation of a double precision number",
                            );
                        };
                        // float8 -> numeric keeps 15 significant digits
                        let d = Numeric::from_f64_15(f).unwrap_or_else(Numeric::zero);
                        self.next(env, node, &Value::Number(d), found)
                    }
                    _ => self.fail(
                        "22036",
                        "jsonpath item method .double() can only be applied to a string or numeric value",
                    ),
                }
            }
            Method::KeyValue => {
                if unwrap && jb.is_array() {
                    return self.elements(env, Some(node), jb, found, false);
                }
                self.key_value(env, node, jb, found)
            }
        }
    }

    fn key_value(&mut self, env: Env, node: &Node, jb: &Value, mut found: F) -> R {
        let Value::Object(o) = jb else {
            return self.fail("2203C", "jsonpath item method .keyvalue() can only be applied to an object");
        };
        if o.is_empty() {
            return Ok(Res::NotFound);
        }
        let has_next = node.next.is_some();
        let here = match env.id {
            0 | 1 => {
                let all = self.offsets[env.id as usize].get_or_insert_with(|| all_offsets(env.base));
                all.get(&(jb as *const Value as usize)).copied().unwrap_or(0)
            }
            _ => container_offset(env.base, jb),
        };
        let id = here as i64 + env.id * 10_000_000_000;
        let mut res = Res::NotFound;
        for (k, v) in o.iter() {
            res = Res::Ok;
            if !has_next && found.is_none() {
                break;
            }
            let mut obj = Object::new();
            obj.insert("id".into(), Value::from_i64(id));
            obj.insert("key".into(), Value::String(k.clone()));
            obj.insert("value".into(), v.clone());
            let obj = Value::Object(obj);
            let made = self.next_id;
            self.next_id += 1;
            res = self.next(Env { base: &obj, id: made, ..env }, node, &obj, found.as_deref_mut())?;
            if res == Res::Error || (res == Res::Ok && found.is_none()) {
                break;
            }
        }
        Ok(res)
    }

    /// A predicate against `jb`.
    fn boolean(&mut self, env: Env, node: &Node, jb: &Value) -> Result<Tri, PathError> {
        self.deep()?;
        let and_or = |res: Tri, res2: Tri, stop: Tri| if res2 == stop { res } else { res2 };
        match &node.op {
            Op::And(l, r) => {
                let res = self.boolean(env, l, jb)?;
                if res == Tri::False {
                    return Ok(Tri::False);
                }
                // a second operand is checked even if the first was unknown
                let res2 = self.boolean(env, r, jb)?;
                Ok(and_or(res, res2, Tri::True))
            }
            Op::Or(l, r) => {
                let res = self.boolean(env, l, jb)?;
                if res == Tri::True {
                    return Ok(Tri::True);
                }
                let res2 = self.boolean(env, r, jb)?;
                Ok(and_or(res, res2, Tri::False))
            }
            Op::Not(a) => Ok(match self.boolean(env, a, jb)? {
                Tri::Unknown => Tri::Unknown,
                Tri::True => Tri::False,
                Tri::False => Tri::True,
            }),
            Op::IsUnknown(a) => Ok(if self.boolean(env, a, jb)? == Tri::Unknown { Tri::True } else { Tri::False }),
            Op::Cmp(c, l, r) => self.predicate_over(env, node, l, Some(r), jb, true, Some(*c)),
            Op::StartsWith(l, r) => self.predicate_over(env, node, l, Some(r), jb, false, None),
            Op::LikeRegex(l, ..) => self.predicate_over(env, node, l, None, jb, false, None),
            Op::Exists(a) => {
                if self.lax {
                    let res = self.item_quiet(env, a, jb, false, None)?;
                    Ok(match res {
                        Res::Error => Tri::Unknown,
                        Res::Ok => Tri::True,
                        Res::NotFound => Tri::False,
                    })
                } else {
                    // strict mode must see every item, to know there is no error among them
                    let mut vals = vec![];
                    let res = self.item_quiet(env, a, jb, false, Some(&mut vals))?;
                    Ok(if res == Res::Error {
                        Tri::Unknown
                    } else if vals.is_empty() {
                        Tri::False
                    } else {
                        Tri::True
                    })
                }
            }
            _ => Err(("XX000", "invalid boolean jsonpath item type".into())),
        }
    }

    /// A comparison, `starts with` or `like_regex` of the items `l` and `r` give: over every
    /// pair (lax mode stops at a true one, strict mode at an unknown one).
    #[allow(clippy::too_many_arguments)]
    fn predicate_over(
        &mut self,
        env: Env,
        node: &Node,
        l: &Node,
        r: Option<&Node>,
        jb: &Value,
        unwrap_right: bool,
        cmp: Option<Cmp>,
    ) -> Result<Tri, PathError> {
        let mut lseq = vec![];
        if self.item_quiet(env, l, jb, true, Some(&mut lseq))? == Res::Error {
            return Ok(Tri::Unknown);
        }
        let mut rseq = vec![];
        if let Some(r) = r
            && self.item_quiet(env, r, jb, unwrap_right, Some(&mut rseq))? == Res::Error
        {
            return Ok(Tri::Unknown);
        }
        let regex = match &node.op {
            Op::LikeRegex(_, _, _, re) => Some(re),
            _ => None,
        };
        let strict = !self.lax;
        let (mut error, mut hit) = (false, false);
        for lv in &lseq {
            let rights: Vec<Option<&Value>> = if r.is_some() { rseq.iter().map(Some).collect() } else { vec![None] };
            for rv in rights {
                let res = match (&node.op, cmp, rv) {
                    (_, Some(c), Some(rv)) => compare(c, lv, rv, self.tz)?,
                    (Op::StartsWith(..), _, Some(rv)) => match (lv, rv) {
                        (Value::String(w), Value::String(i)) => {
                            if w.as_bytes().starts_with(i.as_bytes()) {
                                Tri::True
                            } else {
                                Tri::False
                            }
                        }
                        _ => Tri::Unknown,
                    },
                    (Op::LikeRegex(..), ..) => match (lv, &regex) {
                        (Value::String(s), Some(re)) => {
                            if re.is_match(s, (self.stack, self.budget)).map_err(|m| ("2201B", m))? {
                                Tri::True
                            } else {
                                Tri::False
                            }
                        }
                        _ => Tri::Unknown,
                    },
                    _ => Tri::Unknown,
                };
                match res {
                    Tri::Unknown => {
                        if strict {
                            return Ok(Tri::Unknown);
                        }
                        error = true;
                    }
                    Tri::True => {
                        if !strict {
                            return Ok(Tri::True);
                        }
                        hit = true;
                    }
                    Tri::False => {}
                }
            }
        }
        Ok(if hit {
            Tri::True
        } else if error {
            Tri::Unknown
        } else {
            Tri::False
        })
    }
}

/// Runs `path` over `target`; the items it found (when `collect`) and how it ended.
fn run(
    path: &Path,
    target: &Value,
    vars: &Value,
    throw: bool,
    collect: bool,
    tz: bool,
) -> Result<(Res, Vec<Value>), PathError> {
    run_capped(path, target, vars, throw, collect, tz, MAX_ITEMS)
}

/// `run`, with `max_items` for what the walk may hold (a test's way to reach the limit).
fn run_capped(
    path: &Path,
    target: &Value,
    vars: &Value,
    throw: bool,
    collect: bool,
    tz: bool,
    max_items: u64,
) -> Result<(Res, Vec<Value>), PathError> {
    if !vars.is_object() {
        return Err(("22023", "\"vars\" argument is not an object".into()));
    }
    with_stack(&|budget| walk(path, target, vars, throw, collect, tz, budget, max_items))
}

#[allow(clippy::too_many_arguments)]
fn walk(
    path: &Path,
    target: &Value,
    vars: &Value,
    throw: bool,
    collect: bool,
    tz: bool,
    budget: usize,
    max_items: u64,
) -> Result<(Res, Vec<Value>), PathError> {
    let mut ex = Exec {
        lax: path.lax,
        ignore_structural: path.lax,
        throw,
        root: target,
        vars,
        last_size: -1,
        next_id: 2,
        offsets: [None, None],
        stack: super::stack_at(),
        budget,
        tz,
        steps: std::cell::Cell::new(0),
        items: std::cell::Cell::new(0),
        max_items,
    };
    let env = Env { cur: target, base: target, id: 0 };
    let mut found = vec![];
    if !path.lax && !collect {
        // strict mode must see all the items, to know there is no error among them
        let res = ex.item(env, &path.expr, target, Some(&mut found))?;
        let res = if res == Res::Error {
            res
        } else if found.is_empty() {
            Res::NotFound
        } else {
            Res::Ok
        };
        return Ok((res, vec![]));
    }
    let res = ex.item(env, &path.expr, target, if collect { Some(&mut found) } else { None })?;
    Ok((res, found.into_iter().map(Value::finish).collect()))
}

/// `jsonb_path_query_array(target, path, vars, silent)`: the items the path finds. With
/// `silent`, errors that Postgres leaves out (a missing key or a wrong type in strict mode,
/// arithmetic on non-numbers, ...) end the search instead of failing, and what was found before
/// them stays. `vars` is a JSON object: the path's `$name`s. `tz` is what the `_tz` functions
/// are: a date or time may be compared with a timestamp with a time zone (in the session's).
pub fn query_value(path: &Path, target: &Value, vars: &Value, silent: bool, tz: bool) -> Result<Vec<Value>, PathError> {
    Ok(run(path, target, vars, !silent, true, tz)?.1)
}

/// `jsonb_path_exists` and `@?`: whether the path finds any item; None (NULL) if it ended in an
/// error that `silent` suppressed. `@?` is silent, with no variables.
pub fn exists_value(
    path: &Path,
    target: &Value,
    vars: &Value,
    silent: bool,
    tz: bool,
) -> Result<Option<bool>, PathError> {
    let (res, _) = run(path, target, vars, !silent, false, tz)?;
    Ok(if res == Res::Error { None } else { Some(res == Res::Ok) })
}

/// `jsonb_path_match` and `@@`: the path is a predicate and this is its one boolean result
/// (None, NULL, if it is unknown). Anything else is an error (22038), or NULL if `silent`.
pub fn predicate_value(
    path: &Path,
    target: &Value,
    vars: &Value,
    silent: bool,
    tz: bool,
) -> Result<Option<bool>, PathError> {
    let (_, found) = run(path, target, vars, !silent, true, tz)?;
    match found.as_slice() {
        [Value::Bool(b)] => return Ok(Some(*b)),
        [Value::Null] => return Ok(None),
        _ => {}
    }
    if silent { Ok(None) } else { Err(("22038", "single boolean result is expected".into())) }
}

/// `query_value` over serde_json values (the numbers of which are floats: digits past the 17th
/// are not there to be kept, and a result past what a float holds is an error).
pub fn query(path: &Path, target: &Json, vars: &Json, silent: bool) -> Result<Vec<Json>, PathError> {
    query_value(path, &Value::from_json(target), &Value::from_json(vars), silent, false)?
        .iter()
        .map(Value::to_json)
        .collect()
}

/// `exists_value` over serde_json values.
pub fn exists(path: &Path, target: &Json, vars: &Json, silent: bool) -> Result<Option<bool>, PathError> {
    exists_value(path, &Value::from_json(target), &Value::from_json(vars), silent, false)
}

/// `predicate_value` over serde_json values.
pub fn predicate(path: &Path, target: &Json, vars: &Json, silent: bool) -> Result<Option<bool>, PathError> {
    predicate_value(path, &Value::from_json(target), &Value::from_json(vars), silent, false)
}

// ---------------------------------------------------------------------------------------------
// the text of a path (jsonpath_out), as `jsonpath::text` shows it

use value::escape as escape_json;

/// How tightly an operator binds, for where the text needs parentheses.
fn priority(op: &Op) -> u8 {
    match op {
        Op::Or(..) => 0,
        Op::And(..) => 1,
        Op::Cmp(..) | Op::StartsWith(..) => 2,
        Op::Bin(Ar::Add | Ar::Sub, ..) => 3,
        Op::Bin(..) => 4,
        Op::Plus(_) | Op::Minus(_) => 5,
        _ => 6,
    }
}

fn print(n: &Node, mut in_key: bool, mut brackets: bool, out: &mut String) {
    let mut cur = n;
    loop {
        let binary = |out: &mut String, a: &Node, name: &str, b: &Node| {
            let mine = priority(&cur.op);
            if brackets {
                out.push('(');
            }
            print(a, false, priority(&a.op) <= mine, out);
            out.push(' ');
            out.push_str(name);
            out.push(' ');
            print(b, false, priority(&b.op) <= mine, out);
            if brackets {
                out.push(')');
            }
        };
        match &cur.op {
            Op::Null => out.push_str("null"),
            Op::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Op::Num(d) => {
                if cur.next.is_some() {
                    out.push('(');
                }
                out.push_str(&d.to_string());
                if cur.next.is_some() {
                    out.push(')');
                }
            }
            Op::Str(s) => escape_json(s, out),
            Op::Var(v) => {
                out.push('$');
                escape_json(v, out);
            }
            Op::Root => out.push('$'),
            Op::Current => out.push('@'),
            Op::Last => out.push_str("last"),
            Op::Key(k) => {
                if in_key {
                    out.push('.');
                }
                escape_json(k, out);
            }
            Op::AnyKey => {
                if in_key {
                    out.push('.');
                }
                out.push('*');
            }
            Op::AnyArray => out.push_str("[*]"),
            Op::Index(elems) => {
                out.push('[');
                for (i, (from, to)) in elems.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    print(from, false, false, out);
                    if let Some(to) = to {
                        out.push_str(" to ");
                        print(to, false, false, out);
                    }
                }
                out.push(']');
            }
            Op::Any(first, last) => {
                if in_key {
                    out.push('.');
                }
                let l = |v: u32| if v == u32::MAX { "last".to_string() } else { v.to_string() };
                out.push_str(&match (*first, *last) {
                    (0, u32::MAX) => "**".to_string(),
                    (f, t) if f == t => format!("**{{{}}}", l(f)),
                    (f, t) => format!("**{{{} to {}}}", l(f), l(t)),
                });
            }
            Op::Filter(p) => {
                out.push_str("?(");
                print(p, false, false, out);
                out.push(')');
            }
            Op::Bin(op, a, b) => binary(out, a, ar_name(*op), b),
            Op::Cmp(op, a, b) => binary(
                out,
                a,
                match op {
                    Cmp::Eq => "==",
                    Cmp::Ne => "!=",
                    Cmp::Lt => "<",
                    Cmp::Le => "<=",
                    Cmp::Gt => ">",
                    Cmp::Ge => ">=",
                },
                b,
            ),
            Op::And(a, b) => binary(out, a, "&&", b),
            Op::Or(a, b) => binary(out, a, "||", b),
            Op::StartsWith(a, b) => binary(out, a, "starts with", b),
            Op::Not(a) => {
                out.push_str("!(");
                print(a, false, false, out);
                out.push(')');
            }
            Op::Exists(a) => {
                out.push_str("exists (");
                print(a, false, false, out);
                out.push(')');
            }
            Op::IsUnknown(a) => {
                out.push('(');
                print(a, false, false, out);
                out.push_str(") is unknown");
            }
            Op::Plus(a) | Op::Minus(a) => {
                if brackets {
                    out.push('(');
                }
                out.push(if matches!(cur.op, Op::Plus(_)) { '+' } else { '-' });
                print(a, false, priority(&a.op) <= 5, out);
                if brackets {
                    out.push(')');
                }
            }
            Op::LikeRegex(a, pattern, flags, _) => {
                if brackets {
                    out.push('(');
                }
                print(a, false, true, out);
                out.push_str(" like_regex ");
                escape_json(pattern, out);
                if *flags != 0 {
                    out.push_str(" flag \"");
                    for (bit, c) in [(RE_I, 'i'), (RE_S, 's'), (RE_M, 'm'), (RE_X, 'x'), (RE_Q, 'q')] {
                        if flags & bit != 0 {
                            out.push(c);
                        }
                    }
                    out.push('"');
                }
                if brackets {
                    out.push(')');
                }
            }
            Op::Method(m) => {
                out.push('.');
                out.push_str(method_name(*m));
                out.push_str("()");
            }
            Op::Datetime(t) => {
                out.push_str(".datetime(");
                if let Some(t) = t {
                    escape_json(t, out);
                }
                out.push(')');
            }
        }
        match &cur.next {
            Some(next) => {
                cur = next;
                in_key = true;
                brackets = true;
            }
            None => return,
        }
    }
}

impl fmt::Display for Path {
    /// The path as `SELECT '...'::jsonpath` shows it: `strict` if it is, keys quoted, an
    /// operator's parentheses where they're needed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let render = || {
            let mut out = String::new();
            if !self.lax {
                out.push_str("strict ");
            }
            print(&self.expr, false, true, &mut out);
            out
        };
        // a long path may nest deeper than this thread's stack: then a thread of its own prints it
        let text = if self.nodes <= 200 {
            render()
        } else {
            BIG_STACKS
                .iter()
                .find_map(|&size| {
                    std::thread::scope(|scope| {
                        std::thread::Builder::new().stack_size(size).spawn_scoped(scope, render).ok().map(|h| h.join())
                    })
                })
                .and_then(Result::ok)
                .unwrap_or_else(render)
        };
        f.write_str(&text)
    }
}

#[cfg(test)]
mod tests;
