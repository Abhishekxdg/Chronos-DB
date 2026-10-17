//! Postgres's regular expressions (Henry Spencer's "advanced" REs), as `like_regex` runs them: the
//! syntax of `regcomp.c` and `regc_lex.c` and its error messages, and a matcher that answers
//! whether any part of a string matches.
//!
//! What a pattern may say, and how this reads it, follows Postgres's scanner token by token: the
//! three flavours (`(?e)` EREs, `(?b)` BREs, `(?q)` and `***=` literal strings, AREs otherwise), the
//! embedded options `(?ixmnpswtcq)` and director `***:`, `(?: )`, lookahead and lookbehind
//! `(?= ) (?! ) (?<= ) (?<! )`, comments `(?# )`, lazy quantifiers, bounds up to 255 (`{` that does
//! not start a bound is a plain character), back-references, the constraint escapes `\A \Z \m \M
//! \y \Y`, character-entry escapes (`\x41`, `\uXXXX`, `\UXXXXXXXX`, octal `\101`, `\cA`, `\e`, `\a`), the class shorthands
//! `\d \s \w` and their complements (also inside brackets), and brackets with ranges, classes
//! (`[:alpha:]`, `[:word:]`, ...), collating elements `[.a.]`, equivalence classes `[=a=]` and the
//! word edges `[[:<:]]` `[[:>:]]`. Classes and the case of a character are a C.UTF-8 database's
//! (see `ctype`).
//!
//! A pattern without back-references is run as an NFA over the whole string at once, so the time is
//! linear in the string however the pattern nests (Postgres's is a DFA); one with back-references
//! is backtracked, in a bounded number of steps.
//! ponytail: Postgres answers "regular expression is too complex" at a size of its own; here it is
//! a million instructions of program, or ten million steps of a backtrack.

use super::ctype;
use super::stack_spent;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// flags

/// `ARE` syntax: lazy quantifiers, `(?...)`, escapes.
const ADVF: u32 = 1;
/// `ERE` syntax (with ADVF: `ARE`).
const EXTENDED: u32 = 2;
/// the pattern is a literal string
const QUOTE: u32 = 4;
const ICASE: u32 = 8;
const EXPANDED: u32 = 16;
/// `.` and `[^...]` don't match a newline
const NLSTOP: u32 = 32;
/// `^` and `$` match at newlines
const NLANCH: u32 = 64;
const ADVANCED: u32 = ADVF | EXTENDED;
const NEWLINE: u32 = NLSTOP | NLANCH;

/// What `like_regex`'s flags ask of the engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Flags {
    /// flag `i`
    pub icase: bool,
    /// flag `s`: `.` matches a newline
    pub dotall: bool,
    /// flag `m`: `^` and `$` match at each line
    pub multiline: bool,
    /// flag `q`: the pattern is a literal string
    pub quote: bool,
}

/// How much program a pattern may compile to, and how many steps a backtrack may take.
const MAX_PROGRAM: usize = 1 << 20;
const MAX_STEPS: u64 = 10_000_000;
/// `{m,n}` bounds up to this.
const DUPMAX: u32 = 255;
/// the largest character code a pattern may name
const CHR_MAX: u32 = 0x7fff_fffe;

// ---------------------------------------------------------------------------------------------
// errors, as regerror words them

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum E {
    BadPat,
    Collate,
    CType,
    Escape,
    SubReg,
    Brack,
    Paren,
    Brace,
    BadBr,
    Range,
    BadRpt,
    BadOpt,
    TooBig,
}

impl E {
    fn text(self) -> &'static str {
        match self {
            E::BadPat => "invalid regexp (reg version 0.8)",
            E::Collate => "invalid collating element",
            E::CType => "invalid character class",
            E::Escape => "invalid escape \\ sequence",
            E::SubReg => "invalid backreference number",
            E::Brack => "brackets [] not balanced",
            E::Paren => "parentheses () not balanced",
            E::Brace => "braces {} not balanced",
            E::BadBr => "invalid repetition count(s)",
            E::Range => "invalid character range",
            E::BadRpt => "quantifier operand invalid",
            E::BadOpt => "invalid embedded option",
            E::TooBig => "regular expression is too complex",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// what a bracket, a class escape and a dot match

/// The classes `[[:name:]]` names, and the ones an escape stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Alnum,
    Alpha,
    Ascii,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
    /// `\w`: a letter, a digit or `_`
    Word,
}

impl Class {
    fn named(name: &str) -> Option<Class> {
        Some(match name {
            "alnum" => Class::Alnum,
            "alpha" => Class::Alpha,
            "ascii" => Class::Ascii,
            "blank" => Class::Blank,
            "cntrl" => Class::Cntrl,
            "digit" => Class::Digit,
            "graph" => Class::Graph,
            "lower" => Class::Lower,
            "print" => Class::Print,
            "punct" => Class::Punct,
            "space" => Class::Space,
            "upper" => Class::Upper,
            "xdigit" => Class::Xdigit,
            "word" => Class::Word,
            _ => return None,
        })
    }

    fn has(self, c: u32) -> bool {
        match self {
            Class::Alnum => ctype::is_alnum(c),
            Class::Alpha => ctype::is_alpha(c),
            Class::Ascii => c < 128,
            Class::Blank => ctype::is_blank(c),
            Class::Cntrl => ctype::is_cntrl(c),
            Class::Digit => ctype::is_digit(c),
            Class::Graph => ctype::is_graph(c),
            Class::Lower => ctype::is_lower(c),
            Class::Print => ctype::is_print(c),
            Class::Punct => ctype::is_punct(c),
            Class::Space => ctype::is_space(c),
            Class::Upper => ctype::is_upper(c),
            Class::Xdigit => ctype::is_xdigit(c),
            Class::Word => ctype::is_word(c),
        }
    }
}

/// A set of characters: the chars and ranges a bracket lists, the classes it names, the complements
/// of classes (`\D`), and whether the whole is complemented (`[^...]`).
#[derive(Clone, Debug, Default)]
struct Set {
    ranges: Vec<(u32, u32)>,
    classes: Vec<Class>,
    not_classes: Vec<Class>,
    negated: bool,
}

impl Set {
    fn chars(list: &[u32]) -> Set {
        let mut s = Set::default();
        for &c in list {
            s.ranges.push((c, c));
        }
        s
    }

    fn has(&self, c: u32) -> bool {
        let hit = self.ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi)
            || self.classes.iter().any(|k| k.has(c))
            || self.not_classes.iter().any(|k| !k.has(c));
        hit != self.negated
    }
}

// ---------------------------------------------------------------------------------------------
// the syntax tree

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Assertion {
    /// `\A`, and `^`
    Bos,
    /// `\Z`, and `$`
    Eos,
    /// `^` when newlines are anchors: the start, or just after a newline
    Bol,
    /// `$` when newlines are anchors: the end, or just before a newline
    Eol,
    /// `\m`
    WordStart,
    /// `\M`
    WordEnd,
    /// `\y`
    WordEdge,
    /// `\Y`
    NotWordEdge,
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Char(u32),
    Set(Set),
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
    Group(Box<Node>, usize),
    Backref(usize),
    /// the group has matched (else the whole pattern fails): what a quantified back-reference asks first
    Require(usize),
    Assert(Assertion),
    Look {
        behind: bool,
        negative: bool,
        node: Box<Node>,
    },
}

// ---------------------------------------------------------------------------------------------
// the scanner (regc_lex.c)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum T {
    /// the start of the pattern: no token yet
    Empty,
    Eos,
    Plain,
    Digit,
    Bar,
    Star,
    Plus,
    Quest,
    LBrace,
    RBrace,
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Caret,
    Dollar,
    WordStart,
    WordEnd,
    Lacon,
    Backref,
    SBegin,
    SEnd,
    WBdry,
    NWBdry,
    /// `\d`, `\s` or `\w`
    CClass,
    /// `\D`, `\S` or `\W`
    CClassC,
    /// the `[:` of a class name
    Ccl,
    Collel,
    EClass,
    End,
    Range,
}

/// Where the scanner is: what it reads the next character as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Con {
    Ere,
    Bre,
    /// a literal string
    Q,
    /// an ERE bound `{..}`
    Ebnd,
    /// a BRE bound `\{..\}`
    Bbnd,
    Brack,
    /// a collating element `[. .]`
    Cel,
    /// an equivalence class `[= =]`
    Ecl,
    /// a character class `[: :]`
    Ccl,
}

const LATYPE_AHEAD_POS: u32 = 0;
const LATYPE_AHEAD_NEG: u32 = 1;
const LATYPE_BEHIND_POS: u32 = 2;
const LATYPE_BEHIND_NEG: u32 = 3;

struct P {
    src: Vec<u32>,
    now: usize,
    /// how deep the parser is
    depth: usize,
    cflags: u32,
    lexcon: Con,
    nexttype: T,
    nextvalue: u32,
    lasttype: T,
    nsubexp: usize,
    /// whether group n has been closed, for back-references
    closed: Vec<bool>,
    err: Option<E>,
    /// where the stack stood and how much of it may be used (see `stack_spent`)
    stack: (usize, usize),
}

type PR<X> = Result<X, E>;

fn is_digit_ascii(c: u32) -> bool {
    (0x30..=0x39).contains(&c)
}

fn chars(s: &str) -> Vec<u32> {
    s.chars().map(|c| c as u32).collect()
}

impl P {
    fn ateos(&self) -> bool {
        self.now >= self.src.len()
    }

    /// the character `k` places on (or None at the end)
    fn at(&self, k: usize) -> Option<u32> {
        self.src.get(self.now + k).copied()
    }

    fn next1(&self, c: char) -> bool {
        self.at(0) == Some(c as u32)
    }

    fn next2(&self, a: char, b: char) -> bool {
        self.at(0) == Some(a as u32) && self.at(1) == Some(b as u32)
    }

    /// Sets the error, unless one is set; the token ahead becomes the end.
    fn fail(&mut self, e: E) {
        self.nexttype = T::Eos;
        if self.err.is_none() {
            self.err = Some(e);
        }
    }

    fn ise(&self) -> bool {
        self.err.is_some()
    }

    fn see(&self, t: T) -> bool {
        self.nexttype == t
    }

    fn lasttype(&self, t: T) -> bool {
        self.lasttype == t
    }

    fn tok(&mut self, t: T, v: u32) {
        self.nexttype = t;
        self.nextvalue = v;
    }

    /// Skips blanks and comments (expanded syntax).
    fn skip(&mut self) {
        loop {
            while self.at(0).is_some_and(ctype::is_space) {
                self.now += 1;
            }
            if self.at(0) != Some('#' as u32) {
                break;
            }
            while self.at(0).is_some_and(|c| c != '\n' as u32) {
                self.now += 1;
            }
        }
    }

    /// Reads the director and the embedded options at the start.
    fn prefixes(&mut self) {
        if self.cflags & QUOTE != 0 {
            return;
        }
        if self.src.len() >= 4
            && self.at(0) == Some('*' as u32)
            && self.at(1) == Some('*' as u32)
            && self.at(2) == Some('*' as u32)
        {
            match char::from_u32(self.src[3]) {
                Some('?') => return self.fail(E::BadPat),
                Some('=') => {
                    self.cflags |= QUOTE;
                    self.cflags &= !(ADVANCED | EXPANDED | NEWLINE);
                    self.now += 4;
                    return;
                }
                Some(':') => {
                    self.cflags |= ADVANCED;
                    self.now += 4;
                }
                _ => return self.fail(E::BadRpt),
            }
        }
        if self.cflags & ADVANCED != ADVANCED {
            return;
        }
        if self.src.len() - self.now >= 3 && self.next2('(', '?') && self.at(2).is_some_and(ctype::is_alpha) {
            self.now += 2;
            while let Some(c) = self.at(0).filter(|&c| ctype::is_alpha(c)) {
                match char::from_u32(c).unwrap_or(' ') {
                    'b' => self.cflags &= !(ADVANCED | QUOTE),
                    'c' => self.cflags &= !ICASE,
                    'e' => {
                        self.cflags |= EXTENDED;
                        self.cflags &= !(ADVF | QUOTE);
                    }
                    'i' => self.cflags |= ICASE,
                    'm' | 'n' => self.cflags |= NEWLINE,
                    'p' => {
                        self.cflags |= NLSTOP;
                        self.cflags &= !NLANCH;
                    }
                    'q' => {
                        self.cflags |= QUOTE;
                        self.cflags &= !ADVANCED;
                    }
                    's' => self.cflags &= !NEWLINE,
                    't' => self.cflags &= !EXPANDED,
                    'w' => {
                        self.cflags &= !NLSTOP;
                        self.cflags |= NLANCH;
                    }
                    'x' => self.cflags |= EXPANDED,
                    _ => return self.fail(E::BadOpt),
                }
                self.now += 1;
            }
            if !self.next1(')') {
                return self.fail(E::BadOpt);
            }
            self.now += 1;
            if self.cflags & QUOTE != 0 {
                self.cflags &= !(EXPANDED | NEWLINE);
            }
        }
    }

    fn lexstart(&mut self) {
        self.prefixes();
        if self.ise() {
            return;
        }
        self.lexcon = if self.cflags & QUOTE != 0 {
            Con::Q
        } else if self.cflags & EXTENDED != 0 {
            Con::Ere
        } else {
            Con::Bre
        };
        self.nexttype = T::Empty;
        self.next();
    }

    /// `n` digits of `base` (between `minlen` and `maxlen`), as lexdigits reads them.
    fn lexdigits(&mut self, base: u32, minlen: usize, maxlen: usize) -> u32 {
        let mut n: u32 = 0;
        let mut len = 0;
        while len < maxlen && !self.ateos() {
            let c = self.src[self.now];
            self.now += 1;
            let d = char::from_u32(c).and_then(|ch| ch.to_digit(16)).filter(|_| c < 128);
            let d = match d {
                Some(d) if d < base => d,
                _ => {
                    self.now -= 1;
                    break;
                }
            };
            n = n.wrapping_mul(base).wrapping_add(d);
            len += 1;
        }
        if len < minlen {
            self.fail(E::Escape);
        }
        n
    }

    /// The escape after a backslash (only in AREs): the token it is.
    fn lexescape(&mut self) {
        let c = self.src[self.now];
        self.now += 1;
        // (an escaped letter outside ASCII is itself, as Postgres reads it)
        if c >= 128 || !ctype::is_alnum(c) {
            return self.tok(T::Plain, c);
        }
        let ch = char::from_u32(c).unwrap_or(' ');
        match ch {
            'a' => self.tok(T::Plain, 7),
            'A' => self.tok(T::SBegin, 0),
            'b' => self.tok(T::Plain, 8),
            'B' => self.tok(T::Plain, '\\' as u32),
            'c' => {
                if self.ateos() {
                    return self.fail(E::Escape);
                }
                let v = self.src[self.now] & 0o37;
                self.now += 1;
                self.tok(T::Plain, v);
            }
            'd' => self.tok(T::CClass, 'd' as u32),
            'D' => self.tok(T::CClassC, 'd' as u32),
            'e' => self.tok(T::Plain, 0o33),
            'f' => self.tok(T::Plain, 0x0c),
            'm' => self.tok(T::WordStart, 0),
            'M' => self.tok(T::WordEnd, 0),
            'n' => self.tok(T::Plain, 0x0a),
            'r' => self.tok(T::Plain, 0x0d),
            's' => self.tok(T::CClass, 's' as u32),
            'S' => self.tok(T::CClassC, 's' as u32),
            't' => self.tok(T::Plain, 0x09),
            'u' => {
                let v = self.lexdigits(16, 4, 4);
                if self.ise() || v > CHR_MAX {
                    return self.fail(E::Escape);
                }
                self.tok(T::Plain, v);
            }
            'U' => {
                let v = self.lexdigits(16, 8, 8);
                if self.ise() || v > CHR_MAX {
                    return self.fail(E::Escape);
                }
                self.tok(T::Plain, v);
            }
            'v' => self.tok(T::Plain, 0x0b),
            'w' => self.tok(T::CClass, 'w' as u32),
            'W' => self.tok(T::CClassC, 'w' as u32),
            'x' => {
                let v = self.lexdigits(16, 1, 255);
                if self.ise() || v > CHR_MAX {
                    return self.fail(E::Escape);
                }
                self.tok(T::Plain, v);
            }
            'y' => self.tok(T::WBdry, 0),
            'Y' => self.tok(T::NWBdry, 0),
            'Z' => self.tok(T::SEnd, 0),
            '1'..='9' => {
                let save = self.now;
                self.now -= 1;
                let v = self.lexdigits(10, 1, 255);
                if self.ise() {
                    return self.fail(E::Escape);
                }
                // the ugly heuristic: one digit is a back-reference, more are if there are that many groups
                if self.now == save || (v > 0 && (v as usize) <= self.nsubexp) {
                    return self.tok(T::Backref, v);
                }
                self.now = save;
                self.octal();
            }
            '0' => self.octal(),
            _ => self.fail(E::Escape),
        }
    }

    /// An octal escape: the digit just read, and up to two more.
    fn octal(&mut self) {
        self.now -= 1;
        let mut v = self.lexdigits(8, 1, 3);
        if self.ise() {
            return self.fail(E::Escape);
        }
        if v > 0xff {
            // one digit too many
            self.now -= 1;
            v >>= 3;
        }
        self.tok(T::Plain, v);
    }

    /// The next token.
    fn next(&mut self) {
        if self.ise() {
            return;
        }
        self.lasttype = self.nexttype;
        if self.cflags & EXPANDED != 0 && matches!(self.lexcon, Con::Ere | Con::Bre | Con::Ebnd | Con::Bbnd) {
            self.skip();
        }
        if self.ateos() {
            match self.lexcon {
                Con::Ere | Con::Bre | Con::Q => self.tok(T::Eos, 0),
                Con::Ebnd | Con::Bbnd => self.fail(E::Brace),
                Con::Brack | Con::Cel | Con::Ecl | Con::Ccl => self.fail(E::Brack),
            }
            return;
        }
        let c = self.src[self.now];
        self.now += 1;
        match self.lexcon {
            Con::Bre => return self.brenext(c),
            Con::Ere => {}
            Con::Q => return self.tok(T::Plain, c),
            Con::Ebnd | Con::Bbnd => return self.boundnext(c),
            Con::Brack => return self.bracknext(c),
            Con::Cel => {
                if c == '.' as u32 && self.next1(']') {
                    self.now += 1;
                    self.lexcon = Con::Brack;
                    self.tok(T::End, '.' as u32);
                } else {
                    self.tok(T::Plain, c);
                }
                return;
            }
            Con::Ecl => {
                if c == '=' as u32 && self.next1(']') {
                    self.now += 1;
                    self.lexcon = Con::Brack;
                    self.tok(T::End, '=' as u32);
                } else {
                    self.tok(T::Plain, c);
                }
                return;
            }
            Con::Ccl => {
                if c == ':' as u32 && self.next1(']') {
                    self.now += 1;
                    self.lexcon = Con::Brack;
                    self.tok(T::End, ':' as u32);
                } else {
                    self.tok(T::Plain, c);
                }
                return;
            }
        }
        // EREs and AREs
        let advf = self.cflags & ADVF != 0;
        let lazy = |p: &mut P| {
            if advf && p.next1('?') {
                p.now += 1;
                0
            } else {
                1
            }
        };
        match char::from_u32(c).unwrap_or('\u{fffd}') {
            '|' => self.tok(T::Bar, 0),
            '*' => {
                let v = lazy(self);
                self.tok(T::Star, v);
            }
            '+' => {
                let v = lazy(self);
                self.tok(T::Plus, v);
            }
            '?' => {
                let v = lazy(self);
                self.tok(T::Quest, v);
            }
            '{' => {
                if self.cflags & EXPANDED != 0 {
                    self.skip();
                }
                if self.ateos() || !is_digit_ascii(self.src[self.now]) {
                    self.tok(T::Plain, c);
                } else {
                    self.lexcon = Con::Ebnd;
                    self.tok(T::LBrace, 0);
                }
            }
            '(' => {
                if advf && self.next1('?') {
                    self.now += 1;
                    if self.ateos() {
                        return self.fail(E::BadRpt);
                    }
                    let k = self.src[self.now];
                    self.now += 1;
                    match char::from_u32(k).unwrap_or(' ') {
                        ':' => self.tok(T::LParen, 0),
                        '#' => {
                            while self.at(0).is_some_and(|c| c != ')' as u32) {
                                self.now += 1;
                            }
                            if !self.ateos() {
                                self.now += 1;
                            }
                            // a comment is nothing: on to the next token
                            self.nexttype = self.lasttype;
                            self.next();
                        }
                        '=' => self.tok(T::Lacon, LATYPE_AHEAD_POS),
                        '!' => self.tok(T::Lacon, LATYPE_AHEAD_NEG),
                        '<' => {
                            if self.ateos() {
                                return self.fail(E::BadRpt);
                            }
                            let k = self.src[self.now];
                            self.now += 1;
                            match char::from_u32(k).unwrap_or(' ') {
                                '=' => self.tok(T::Lacon, LATYPE_BEHIND_POS),
                                '!' => self.tok(T::Lacon, LATYPE_BEHIND_NEG),
                                _ => self.fail(E::BadRpt),
                            }
                        }
                        _ => self.fail(E::BadRpt),
                    }
                    return;
                }
                self.tok(T::LParen, 1);
            }
            ')' => self.tok(T::RParen, c),
            '[' => {
                if self.src.len() - self.now >= 6
                    && self.at(0) == Some('[' as u32)
                    && self.at(1) == Some(':' as u32)
                    && matches!(self.at(2), Some(0x3c | 0x3e))
                    && self.at(3) == Some(':' as u32)
                    && self.at(4) == Some(']' as u32)
                    && self.at(5) == Some(']' as u32)
                {
                    let k = self.src[self.now + 2];
                    self.now += 6;
                    self.tok(if k == '<' as u32 { T::WordStart } else { T::WordEnd }, 0);
                    return;
                }
                self.lexcon = Con::Brack;
                if self.next1('^') {
                    self.now += 1;
                    self.tok(T::LBracket, 0);
                } else {
                    self.tok(T::LBracket, 1);
                }
            }
            '.' => self.tok(T::Dot, 0),
            '^' => self.tok(T::Caret, 0),
            '$' => self.tok(T::Dollar, 0),
            '\\' => {
                if self.ateos() {
                    return self.fail(E::Escape);
                }
                if !advf {
                    // only AREs have escapes beyond a quoted character
                    let k = self.src[self.now];
                    self.now += 1;
                    return self.tok(T::Plain, k);
                }
                self.lexescape();
                if self.ise() {
                    self.fail(E::Escape);
                }
            }
            _ => self.tok(T::Plain, c),
        }
    }

    fn boundnext(&mut self, c: u32) {
        match char::from_u32(c).unwrap_or(' ') {
            '0'..='9' => self.tok(T::Digit, c - '0' as u32),
            ',' => self.tok(T::Comma, 0),
            '}' if self.lexcon == Con::Ebnd => {
                self.lexcon = Con::Ere;
                if self.cflags & ADVF != 0 && self.next1('?') {
                    self.now += 1;
                    self.tok(T::RBrace, 0);
                } else {
                    self.tok(T::RBrace, 1);
                }
            }
            '\\' if self.lexcon == Con::Bbnd && self.next1('}') => {
                self.now += 1;
                self.lexcon = Con::Bre;
                self.tok(T::RBrace, 1);
            }
            _ => self.fail(E::BadBr),
        }
    }

    fn bracknext(&mut self, c: u32) {
        match char::from_u32(c).unwrap_or('\u{fffd}') {
            ']' => {
                if self.lasttype(T::LBracket) {
                    self.tok(T::Plain, c);
                } else {
                    self.lexcon = if self.cflags & EXTENDED != 0 { Con::Ere } else { Con::Bre };
                    self.tok(T::RBracket, 0);
                }
            }
            '\\' => {
                if self.cflags & ADVF == 0 {
                    return self.tok(T::Plain, c);
                }
                if self.ateos() {
                    return self.fail(E::Escape);
                }
                self.lexescape();
                if self.ise() {
                    return self.fail(E::Escape);
                }
                match self.nexttype {
                    T::Plain => {}
                    T::CClass | T::CClassC => {}
                    _ => self.fail(E::Escape),
                }
            }
            '-' => {
                if self.lasttype(T::LBracket) || self.next1(']') {
                    self.tok(T::Plain, c);
                } else {
                    self.tok(T::Range, c);
                }
            }
            '[' => {
                if self.ateos() {
                    return self.fail(E::Brack);
                }
                let k = self.src[self.now];
                self.now += 1;
                match char::from_u32(k).unwrap_or(' ') {
                    '.' => {
                        self.lexcon = Con::Cel;
                        self.tok(T::Collel, 0);
                    }
                    '=' => {
                        self.lexcon = Con::Ecl;
                        self.tok(T::EClass, 0);
                    }
                    ':' => {
                        self.lexcon = Con::Ccl;
                        self.tok(T::Ccl, 0);
                    }
                    _ => {
                        self.now -= 1;
                        self.tok(T::Plain, c);
                    }
                }
            }
            _ => self.tok(T::Plain, c),
        }
    }

    /// A token in a BRE.
    fn brenext(&mut self, c: u32) {
        let ch = char::from_u32(c).unwrap_or('\u{fffd}');
        match ch {
            '*' => {
                if self.lasttype(T::Empty) || self.lasttype(T::LParen) || self.lasttype(T::Caret) {
                    return self.tok(T::Plain, c);
                }
                return self.tok(T::Star, 1);
            }
            '[' => {
                if self.src.len() - self.now >= 6
                    && self.at(0) == Some('[' as u32)
                    && self.at(1) == Some(':' as u32)
                    && matches!(self.at(2), Some(0x3c | 0x3e))
                    && self.at(3) == Some(':' as u32)
                    && self.at(4) == Some(']' as u32)
                    && self.at(5) == Some(']' as u32)
                {
                    let k = self.src[self.now + 2];
                    self.now += 6;
                    return self.tok(if k == '<' as u32 { T::WordStart } else { T::WordEnd }, 0);
                }
                self.lexcon = Con::Brack;
                if self.next1('^') {
                    self.now += 1;
                    return self.tok(T::LBracket, 0);
                }
                return self.tok(T::LBracket, 1);
            }
            '.' => return self.tok(T::Dot, 0),
            '^' => {
                if self.lasttype(T::Empty) || self.lasttype(T::LParen) {
                    return self.tok(T::Caret, 0);
                }
                return self.tok(T::Plain, c);
            }
            '$' => {
                if self.cflags & EXPANDED != 0 {
                    self.skip();
                }
                if self.ateos() || self.next2('\\', ')') {
                    return self.tok(T::Dollar, 0);
                }
                return self.tok(T::Plain, c);
            }
            '\\' => {}
            _ => return self.tok(T::Plain, c),
        }
        if self.ateos() {
            return self.fail(E::Escape);
        }
        let k = self.src[self.now];
        self.now += 1;
        match char::from_u32(k).unwrap_or(' ') {
            '{' => {
                self.lexcon = Con::Bbnd;
                self.tok(T::LBrace, 0);
            }
            '(' => self.tok(T::LParen, 1),
            ')' => self.tok(T::RParen, k),
            '<' => self.tok(T::WordStart, 0),
            '>' => self.tok(T::WordEnd, 0),
            '1'..='9' => self.tok(T::Backref, k - '0' as u32),
            _ => self.tok(T::Plain, k),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// the parser (regcomp.c)

/// The names of the collating elements `[.name.]` (and of equivalence classes): POSIX's.
const CNAMES: &[(&str, u32)] = &[
    ("NUL", 0),
    ("SOH", 1),
    ("STX", 2),
    ("ETX", 3),
    ("EOT", 4),
    ("ENQ", 5),
    ("ACK", 6),
    ("BEL", 7),
    ("alert", 7),
    ("BS", 8),
    ("backspace", 8),
    ("HT", 9),
    ("tab", 9),
    ("LF", 10),
    ("newline", 10),
    ("VT", 11),
    ("vertical-tab", 11),
    ("FF", 12),
    ("form-feed", 12),
    ("CR", 13),
    ("carriage-return", 13),
    ("SO", 14),
    ("SI", 15),
    ("DLE", 16),
    ("DC1", 17),
    ("DC2", 18),
    ("DC3", 19),
    ("DC4", 20),
    ("NAK", 21),
    ("SYN", 22),
    ("ETB", 23),
    ("CAN", 24),
    ("EM", 25),
    ("SUB", 26),
    ("ESC", 27),
    ("IS4", 28),
    ("FS", 28),
    ("IS3", 29),
    ("GS", 29),
    ("IS2", 30),
    ("RS", 30),
    ("IS1", 31),
    ("US", 31),
    ("space", 32),
    ("exclamation-mark", 33),
    ("quotation-mark", 34),
    ("number-sign", 35),
    ("dollar-sign", 36),
    ("percent-sign", 37),
    ("ampersand", 38),
    ("apostrophe", 39),
    ("left-parenthesis", 40),
    ("right-parenthesis", 41),
    ("asterisk", 42),
    ("plus-sign", 43),
    ("comma", 44),
    ("hyphen", 45),
    ("hyphen-minus", 45),
    ("period", 46),
    ("full-stop", 46),
    ("slash", 47),
    ("solidus", 47),
    ("zero", 48),
    ("one", 49),
    ("two", 50),
    ("three", 51),
    ("four", 52),
    ("five", 53),
    ("six", 54),
    ("seven", 55),
    ("eight", 56),
    ("nine", 57),
    ("colon", 58),
    ("semicolon", 59),
    ("less-than-sign", 60),
    ("equals-sign", 61),
    ("greater-than-sign", 62),
    ("question-mark", 63),
    ("commercial-at", 64),
    ("left-square-bracket", 91),
    ("backslash", 92),
    ("reverse-solidus", 92),
    ("right-square-bracket", 93),
    ("circumflex", 94),
    ("circumflex-accent", 94),
    ("underscore", 95),
    ("low-line", 95),
    ("grave-accent", 96),
    ("left-brace", 123),
    ("left-curly-bracket", 123),
    ("vertical-line", 124),
    ("right-brace", 125),
    ("right-curly-bracket", 125),
    ("tilde", 126),
    ("DEL", 127),
];

/// How deep parentheses may nest (Postgres stops at its stack's depth, with the same message).
const MAX_NEST: usize = 10_000;

impl P {
    fn chk(&self) -> PR<()> {
        match self.err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn bad<X>(&mut self, e: E) -> PR<X> {
        self.fail(e);
        Err(self.err.unwrap_or(e))
    }

    fn icase(&self) -> bool {
        self.cflags & ICASE != 0
    }

    /// `EAT(t)`: steps over `t`, if it is the token ahead.
    fn eat(&mut self, t: T) -> bool {
        if self.see(t) {
            self.next();
            !self.ise()
        } else {
            false
        }
    }

    /// Alternatives up to `stopper`: `)` (inside parentheses, where `lacon` says whether they are a
    /// lookaround's) or the end.
    fn parse(&mut self, stopper: T, lacon: bool) -> PR<Node> {
        self.depth += 1;
        if self.depth > MAX_NEST || stack_spent(self.stack.0, self.stack.1) {
            return self.bad(E::TooBig);
        }
        let mut branches = vec![];
        loop {
            branches.push(self.parsebranch(stopper, lacon)?);
            if !self.eat(T::Bar) {
                break;
            }
        }
        self.chk()?;
        if !self.see(stopper) {
            return self.bad(E::Paren);
        }
        self.depth -= 1;
        Ok(if branches.len() == 1 { branches.remove(0) } else { Node::Alt(branches) })
    }

    fn parsebranch(&mut self, stopper: T, lacon: bool) -> PR<Node> {
        let mut items = vec![];
        while !self.see(T::Bar) && !self.see(stopper) && !self.see(T::Eos) {
            items.push(self.parseqatom(lacon)?);
        }
        self.chk()?;
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.remove(0),
            _ => Node::Cat(items),
        })
    }

    /// An atom, or a constraint, and the quantifier after the atom.
    fn parseqatom(&mut self, lacon: bool) -> PR<Node> {
        let direct_backref = self.nexttype == T::Backref;
        let atom = match self.nexttype {
            T::Caret => {
                let a = if self.cflags & NLANCH != 0 { Assertion::Bol } else { Assertion::Bos };
                return self.constraint(Node::Assert(a));
            }
            T::Dollar => {
                let a = if self.cflags & NLANCH != 0 { Assertion::Eol } else { Assertion::Eos };
                return self.constraint(Node::Assert(a));
            }
            T::SBegin => return self.constraint(Node::Assert(Assertion::Bos)),
            T::SEnd => return self.constraint(Node::Assert(Assertion::Eos)),
            T::WordStart => return self.constraint(Node::Assert(Assertion::WordStart)),
            T::WordEnd => return self.constraint(Node::Assert(Assertion::WordEnd)),
            T::WBdry => return self.constraint(Node::Assert(Assertion::WordEdge)),
            T::NWBdry => return self.constraint(Node::Assert(Assertion::NotWordEdge)),
            T::Lacon => {
                let latype = self.nextvalue;
                self.next();
                self.chk()?;
                let inner = self.parse(T::RParen, true)?;
                self.next();
                self.chk()?;
                return Ok(Node::Look {
                    behind: latype >= LATYPE_BEHIND_POS,
                    negative: latype == LATYPE_AHEAD_NEG || latype == LATYPE_BEHIND_NEG,
                    node: Box::new(inner),
                });
            }
            T::Star | T::Plus | T::Quest | T::LBrace => return self.bad(E::BadRpt),
            T::RParen => {
                if self.cflags & ADVANCED != EXTENDED {
                    return self.bad(E::Paren);
                }
                let c = self.nextvalue;
                self.next();
                self.chk()?;
                Node::Char(c)
            }
            T::Plain => {
                let c = self.nextvalue;
                self.next();
                self.chk()?;
                Node::Char(c)
            }
            T::LBracket => {
                let positive = self.nextvalue == 1;
                let n = self.bracket(positive)?;
                self.next();
                self.chk()?;
                n
            }
            T::Dot => {
                let mut s = Set { negated: true, ..Set::default() };
                if self.cflags & NLSTOP != 0 {
                    s.ranges.push((10, 10));
                }
                self.next();
                self.chk()?;
                Node::Set(s)
            }
            T::CClass | T::CClassC => {
                let class = escape_class(self.nextvalue);
                let mut s = Set::default();
                if self.nexttype == T::CClass {
                    s.classes.push(class);
                } else {
                    s.not_classes.push(class);
                }
                self.next();
                self.chk()?;
                Node::Set(s)
            }
            T::LParen => {
                let capture = !lacon && self.nextvalue == 1;
                let mut subno = 0;
                if capture {
                    self.nsubexp += 1;
                    subno = self.nsubexp;
                    if self.closed.len() <= subno {
                        self.closed.resize(subno + 1, false);
                    }
                }
                self.next();
                self.chk()?;
                let inner = self.parse(T::RParen, lacon)?;
                self.next();
                self.chk()?;
                if capture {
                    self.closed[subno] = true;
                    Node::Group(Box::new(inner), subno)
                } else {
                    inner
                }
            }
            T::Backref => {
                let n = self.nextvalue as usize;
                if lacon || !self.closed.get(n).copied().unwrap_or(false) {
                    return self.bad(E::SubReg);
                }
                self.next();
                self.chk()?;
                Node::Backref(n)
            }
            _ => return self.bad(E::BadPat),
        };
        // an atom may be followed by a quantifier
        let (min, max) = match self.nexttype {
            T::Star => {
                self.next();
                (0, None)
            }
            T::Plus => {
                self.next();
                (1, None)
            }
            T::Quest => {
                self.next();
                (0, Some(1))
            }
            T::LBrace => {
                self.next();
                self.chk()?;
                let m = self.scannum()?;
                let n = if self.eat(T::Comma) {
                    let n = if self.see(T::Digit) { Some(self.scannum()?) } else { None };
                    if n.is_some_and(|n| m > n) {
                        return self.bad(E::BadBr);
                    }
                    n
                } else {
                    Some(m)
                };
                self.chk()?;
                if !self.see(T::RBrace) {
                    return self.bad(E::BadBr);
                }
                self.next();
                (m, n)
            }
            _ => (1, Some(1)),
        };
        self.chk()?;
        if max == Some(0) {
            // {0} cancels the atom (a group in it still counts, for a back-reference)
            return Ok(Node::Empty);
        }
        Ok(match atom {
            atom if (min, max) == (1, Some(1)) => atom,
            // a quantifier straight on a back-reference is part of it: a group that has not matched
            // fails it even where no repetition is asked for
            Node::Backref(g) if direct_backref => {
                Node::Cat(vec![Node::Require(g), Node::Repeat(Box::new(Node::Backref(g)), min, max)])
            }
            atom => Node::Repeat(Box::new(atom), min, max),
        })
    }

    /// A constraint: no quantifier may follow it.
    fn constraint(&mut self, n: Node) -> PR<Node> {
        self.next();
        self.chk()?;
        Ok(n)
    }

    fn scannum(&mut self) -> PR<u32> {
        let mut n = 0u32;
        while self.see(T::Digit) && n < DUPMAX {
            n = n * 10 + self.nextvalue;
            self.next();
            self.chk()?;
        }
        if self.see(T::Digit) || n > DUPMAX {
            return self.bad(E::BadBr);
        }
        Ok(n)
    }

    /// The bracket expression the `[` ahead starts, up to its `]`.
    fn bracket(&mut self, positive: bool) -> PR<Node> {
        self.next();
        self.chk()?;
        let mut set = Set::default();
        while !self.see(T::RBracket) && !self.see(T::Eos) {
            self.brackpart(&mut set)?;
        }
        self.chk()?;
        if !positive {
            if self.cflags & NLSTOP != 0 {
                set.ranges.push((10, 10));
            }
            set.negated = true;
        }
        Ok(Node::Set(set))
    }

    /// The name a `[. .]`, `[= =]` or `[: :]` holds, as where it starts and ends.
    fn scanplain(&mut self) -> PR<(usize, usize)> {
        let startp = self.now;
        self.next();
        self.chk()?;
        let mut endp = self.now;
        while self.see(T::Plain) {
            endp = self.now;
            self.next();
            self.chk()?;
        }
        self.next();
        self.chk()?;
        Ok((startp, endp))
    }

    /// The character a collating element name stands for.
    fn element(&mut self, name: &[u32]) -> PR<u32> {
        if name.len() == 1 {
            return Ok(name[0]);
        }
        let text: Option<String> = name.iter().map(|&c| char::from_u32(c)).collect();
        match text.and_then(|t| CNAMES.iter().find(|(n, _)| *n == t).map(|&(_, c)| c)) {
            Some(c) => Ok(c),
            None => self.bad(E::Collate),
        }
    }

    /// `c` and what it is in the other case.
    fn allcases(&self, set: &mut Set, c: u32) {
        let (lc, uc) = (ctype::to_lower(c), ctype::to_upper(c));
        set.ranges.push((lc, lc));
        if lc != uc {
            set.ranges.push((uc, uc));
        }
    }

    fn onechr(&self, set: &mut Set, c: u32) {
        if self.icase() {
            self.allcases(set, c);
        } else {
            set.ranges.push((c, c));
        }
    }

    fn range(&mut self, set: &mut Set, a: u32, b: u32) -> PR<()> {
        if a > b {
            return self.bad(E::Range);
        }
        set.ranges.push((a, b));
        if !self.icase() {
            return Ok(());
        }
        // the range, and the case-equivalents outside it as characters of their own
        let mut extra = 0usize;
        for c in a..=b {
            for cc in [ctype::to_lower(c), ctype::to_upper(c)] {
                if cc != c && (cc < a || cc > b) {
                    extra += 1;
                    if extra > 100_000 {
                        return self.bad(E::TooBig);
                    }
                    set.ranges.push((cc, cc));
                }
            }
        }
        Ok(())
    }

    /// One part of a bracket expression: a character, a range, a class, ...
    fn brackpart(&mut self, set: &mut Set) -> PR<()> {
        let startc = match self.nexttype {
            T::Range => return self.bad(E::Range),
            T::Plain => {
                let c = self.nextvalue;
                self.next();
                self.chk()?;
                if !self.see(T::Range) {
                    self.onechr(set, c);
                    return Ok(());
                }
                self.element(&[c])?
            }
            T::Collel => {
                let (s, e) = self.scanplain()?;
                if s >= e {
                    return self.bad(E::Collate);
                }
                let name = self.src[s..e].to_vec();
                self.element(&name)?
            }
            T::EClass => {
                let (s, e) = self.scanplain()?;
                if s >= e {
                    return self.bad(E::Collate);
                }
                let name = self.src[s..e].to_vec();
                let c = self.element(&name)?;
                self.onechr(set, c);
                return Ok(());
            }
            T::Ccl => {
                let (s, e) = self.scanplain()?;
                if s >= e {
                    return self.bad(E::CType);
                }
                let name: Option<String> = self.src[s..e].iter().map(|&c| char::from_u32(c)).collect();
                let Some(mut class) = name.as_deref().and_then(Class::named) else { return self.bad(E::CType) };
                if self.icase() && matches!(class, Class::Lower | Class::Upper) {
                    class = Class::Alpha;
                }
                set.classes.push(class);
                return Ok(());
            }
            T::CClass | T::CClassC => {
                let class = escape_class(self.nextvalue);
                if self.nexttype == T::CClass {
                    set.classes.push(class);
                } else {
                    set.not_classes.push(class);
                }
                self.next();
                self.chk()?;
                return Ok(());
            }
            _ => return self.bad(E::BadPat),
        };
        let endc = if self.see(T::Range) {
            self.next();
            self.chk()?;
            match self.nexttype {
                T::Plain | T::Range => {
                    let c = self.nextvalue;
                    self.next();
                    self.chk()?;
                    self.element(&[c])?
                }
                T::Collel => {
                    let (s, e) = self.scanplain()?;
                    if s >= e {
                        return self.bad(E::Collate);
                    }
                    let name = self.src[s..e].to_vec();
                    self.element(&name)?
                }
                _ => return self.bad(E::Range),
            }
        } else {
            startc
        };
        self.range(set, startc, endc)
    }
}

fn escape_class(v: u32) -> Class {
    match char::from_u32(v) {
        Some('d') => Class::Digit,
        Some('s') => Class::Space,
        _ => Class::Word,
    }
}

// ---------------------------------------------------------------------------------------------
// the program

#[derive(Clone, Debug)]
enum Inst {
    Char(u32),
    Set(usize),
    Split(usize, usize),
    Jmp(usize),
    Assert(Assertion),
    Look(usize),
    Save(usize),
    Backref(usize),
    Require(usize),
    /// where a loop's iteration began, and the failure of an iteration that took no character (a
    /// backtrack would go round for ever)
    Mark(usize),
    Check(usize),
    Match,
}

#[derive(Debug)]
struct Prog {
    insts: Vec<Inst>,
    looks: Vec<LookProg>,
}

impl Drop for Prog {
    /// Taken apart with a list: lookarounds nest as deep as a pattern does.
    fn drop(&mut self) {
        let mut work: Vec<Prog> = std::mem::take(&mut self.looks).into_iter().map(|l| l.prog).collect();
        while let Some(mut p) = work.pop() {
            work.extend(std::mem::take(&mut p.looks).into_iter().map(|l| l.prog));
        }
    }
}

#[derive(Debug)]
struct LookProg {
    id: usize,
    behind: bool,
    negative: bool,
    prog: Prog,
}

struct Comp {
    insts: Vec<Inst>,
    looks: Vec<LookProg>,
    sets: Vec<Set>,
    icase: bool,
    backrefs: bool,
    marks: usize,
    looks_made: usize,
    budget: usize,
    stack: (usize, usize),
}

impl Comp {
    fn push(&mut self, i: Inst) -> PR<usize> {
        if self.budget == 0 {
            return Err(E::TooBig);
        }
        self.budget -= 1;
        self.insts.push(i);
        Ok(self.insts.len() - 1)
    }

    fn here(&self) -> usize {
        self.insts.len()
    }

    fn emit(&mut self, n: &Node) -> PR<()> {
        if stack_spent(self.stack.0, self.stack.1) {
            return Err(E::TooBig);
        }
        match n {
            Node::Empty => {}
            Node::Char(c) => {
                let (lc, uc) = (ctype::to_lower(*c), ctype::to_upper(*c));
                if self.icase && (lc != *c || uc != *c) {
                    let mut s = Set::chars(&[lc]);
                    if uc != lc {
                        s.ranges.push((uc, uc));
                    }
                    self.sets.push(s);
                    let i = self.sets.len() - 1;
                    self.push(Inst::Set(i))?;
                } else {
                    self.push(Inst::Char(*c))?;
                }
            }
            Node::Set(s) => {
                self.sets.push(s.clone());
                let i = self.sets.len() - 1;
                self.push(Inst::Set(i))?;
            }
            Node::Cat(v) => {
                for x in v {
                    self.emit(x)?;
                }
            }
            Node::Alt(v) => {
                let mut jumps = vec![];
                for (i, x) in v.iter().enumerate() {
                    if i + 1 < v.len() {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.emit(x)?;
                        jumps.push(self.push(Inst::Jmp(0))?);
                        let next = self.here();
                        self.insts[split] = Inst::Split(split + 1, next);
                    } else {
                        self.emit(x)?;
                    }
                }
                let end = self.here();
                for j in jumps {
                    self.insts[j] = Inst::Jmp(end);
                }
            }
            Node::Repeat(x, min, max) => {
                for _ in 0..*min {
                    self.emit(x)?;
                }
                match max {
                    None => {
                        // x*
                        let split = self.push(Inst::Split(0, 0))?;
                        let mark = if self.backrefs {
                            self.marks += 1;
                            Some(self.marks - 1)
                        } else {
                            None
                        };
                        if let Some(m) = mark {
                            self.push(Inst::Mark(m))?;
                        }
                        self.emit(x)?;
                        if let Some(m) = mark {
                            self.push(Inst::Check(m))?;
                        }
                        self.push(Inst::Jmp(split))?;
                        let end = self.here();
                        self.insts[split] = Inst::Split(split + 1, end);
                    }
                    Some(mx) => {
                        let mut splits = vec![];
                        for _ in *min..*mx {
                            splits.push(self.push(Inst::Split(0, 0))?);
                            self.emit(x)?;
                        }
                        let end = self.here();
                        for s in splits {
                            self.insts[s] = Inst::Split(s + 1, end);
                        }
                    }
                }
            }
            Node::Group(x, g) => {
                self.push(Inst::Save(2 * g))?;
                self.emit(x)?;
                self.push(Inst::Save(2 * g + 1))?;
            }
            Node::Backref(g) => {
                self.push(Inst::Backref(*g))?;
            }
            Node::Require(g) => {
                self.push(Inst::Require(*g))?;
            }
            Node::Assert(a) => {
                self.push(Inst::Assert(*a))?;
            }
            Node::Look { behind, negative, node } => {
                let mut sub = Comp {
                    insts: vec![],
                    looks: vec![],
                    sets: std::mem::take(&mut self.sets),
                    icase: self.icase,
                    backrefs: self.backrefs,
                    marks: self.marks,
                    looks_made: self.looks_made,
                    budget: self.budget,
                    stack: self.stack,
                };
                sub.emit(node)?;
                sub.push(Inst::Match)?;
                self.sets = sub.sets;
                self.marks = sub.marks;
                self.looks_made = sub.looks_made + 1;
                self.budget = sub.budget;
                let id = self.looks_made;
                self.looks.push(LookProg {
                    id,
                    behind: *behind,
                    negative: *negative,
                    prog: Prog { insts: sub.insts, looks: sub.looks },
                });
                let k = self.looks.len() - 1;
                self.push(Inst::Look(k))?;
            }
        }
        Ok(())
    }
}

/// Whether a tree holds a back-reference.
fn has_backref(n: &Node) -> bool {
    let mut work = vec![n];
    while let Some(n) = work.pop() {
        match n {
            Node::Backref(_) | Node::Require(_) => return true,
            Node::Cat(v) | Node::Alt(v) => work.extend(v.iter()),
            Node::Repeat(x, ..) | Node::Group(x, _) => work.push(x),
            Node::Look { node, .. } => work.push(node),
            _ => {}
        }
    }
    false
}

/// A compiled regular expression.
#[derive(Debug)]
pub struct Regex {
    prog: Prog,
    sets: Vec<Set>,
    groups: usize,
    marks: usize,
    backrefs: bool,
    icase: bool,
}

impl Regex {
    /// Compiles `pattern`; the error is Postgres's words for what is wrong with it. `stack` is where
    /// the stack stood, and how much of it the parse may use: a pattern nested deeper than that is
    /// refused (and the caller may try again with more).
    pub fn new(pattern: &str, flags: Flags, stack: (usize, usize)) -> Result<Regex, String> {
        let mut cflags = if flags.quote { QUOTE } else { ADVANCED };
        if flags.icase {
            cflags |= ICASE;
        }
        if !flags.quote {
            if !flags.dotall {
                cflags |= NLSTOP;
            }
            if flags.multiline {
                cflags |= NLANCH;
            }
        }
        let mut p = P {
            src: chars(pattern),
            now: 0,
            depth: 0,
            cflags,
            lexcon: Con::Ere,
            nexttype: T::Empty,
            nextvalue: 0,
            lasttype: T::Empty,
            nsubexp: 0,
            closed: vec![false],
            err: None,
            stack,
        };
        let build = |p: &mut P| -> PR<Regex> {
            p.lexstart();
            p.chk()?;
            let tree = p.parse(T::Eos, false)?;
            let backrefs = has_backref(&tree);
            let mut c = Comp {
                insts: vec![],
                looks: vec![],
                sets: vec![],
                icase: p.icase(),
                backrefs,
                marks: 0,
                looks_made: 0,
                budget: MAX_PROGRAM,
                stack,
            };
            c.emit(&tree)?;
            c.push(Inst::Match)?;
            Ok(Regex {
                prog: Prog { insts: c.insts, looks: c.looks },
                sets: c.sets,
                groups: p.nsubexp,
                marks: c.marks,
                backrefs,
                icase: p.icase(),
            })
        };
        build(&mut p).map_err(|e| e.text().to_string())
    }

    /// Whether any part of `text` matches.
    pub fn is_match(&self, text: &str, stack: (usize, usize)) -> Result<bool, String> {
        let t: Vec<u32> = text.chars().map(|c| c as u32).collect();
        let mut run = Run { re: self, text: &t, memo: HashMap::new(), steps: 0, stack };
        let r = if self.backrefs {
            let mut found = Ok(false);
            for start in 0..=t.len() {
                match run.backtrack(&self.prog, start) {
                    Ok(true) => {
                        found = Ok(true);
                        break;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        found = Err(e);
                        break;
                    }
                }
            }
            found
        } else {
            run.pike(&self.prog, Mode::Search)
        };
        r.map_err(|e| format!("regular expression failed: {}", e.text()))
    }
}

// ---------------------------------------------------------------------------------------------
// the matcher

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// some match anywhere
    Search,
    /// a match that starts here
    At(usize),
    /// a match that ends here
    EndAt(usize),
}

struct Run<'a> {
    re: &'a Regex,
    text: &'a [u32],
    /// what each lookaround answered, by lookaround and place
    memo: HashMap<(usize, usize), bool>,
    steps: u64,
    stack: (usize, usize),
}

/// A set of instruction addresses, in the order they were added.
struct Sparse {
    dense: Vec<usize>,
    mark: Vec<u32>,
    epoch: u32,
}

impl Sparse {
    fn new(n: usize) -> Sparse {
        Sparse { dense: vec![], mark: vec![0; n], epoch: 1 }
    }

    fn clear(&mut self) {
        self.dense.clear();
        self.epoch += 1;
    }

    fn insert(&mut self, pc: usize) -> bool {
        if self.mark[pc] == self.epoch {
            return false;
        }
        self.mark[pc] = self.epoch;
        true
    }
}

const NONE: usize = usize::MAX;

enum Job {
    Try(usize, usize),
    Restore(usize, usize),
}

impl Run<'_> {
    fn assertion(&self, a: Assertion, pos: usize) -> bool {
        let n = self.text.len();
        let prev = if pos > 0 { Some(self.text[pos - 1]) } else { None };
        let next = self.text.get(pos).copied();
        let word = |c: Option<u32>| c.is_some_and(ctype::is_word);
        match a {
            Assertion::Bos => pos == 0,
            Assertion::Eos => pos == n,
            Assertion::Bol => pos == 0 || prev == Some(10),
            Assertion::Eol => pos == n || next == Some(10),
            Assertion::WordStart => !word(prev) && word(next),
            Assertion::WordEnd => word(prev) && !word(next),
            Assertion::WordEdge => (!word(prev) && word(next)) || (word(prev) && !word(next)),
            Assertion::NotWordEdge => word(prev) == word(next),
        }
    }

    /// Whether the lookaround `look` holds at `pos`.
    fn look(&mut self, look: &LookProg, pos: usize) -> Result<bool, E> {
        if stack_spent(self.stack.0, self.stack.1) {
            return Err(E::TooBig);
        }
        let key = (look.id, pos);
        let found = match self.memo.get(&key) {
            Some(&f) => f,
            None => {
                let f = self.pike(&look.prog, if look.behind { Mode::EndAt(pos) } else { Mode::At(pos) })?;
                self.memo.insert(key, f);
                f
            }
        };
        Ok(found != look.negative)
    }

    fn set_has(&self, i: usize, c: u32) -> bool {
        self.re.sets[i].has(c)
    }

    /// Adds the threads that `pc` leads to at `pos` (following the jumps, the assertions, ...) to
    /// `list`; true if one of them is the end of the program.
    fn add(
        &mut self,
        prog: &Prog,
        list: &mut Sparse,
        pc: usize,
        pos: usize,
        stack: &mut Vec<usize>,
    ) -> Result<bool, E> {
        let mut matched = false;
        stack.push(pc);
        while let Some(pc) = stack.pop() {
            if !list.insert(pc) {
                continue;
            }
            match &prog.insts[pc] {
                Inst::Jmp(t) => stack.push(*t),
                Inst::Split(a, b) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                Inst::Assert(a) => {
                    if self.assertion(*a, pos) {
                        stack.push(pc + 1);
                    }
                }
                Inst::Look(k) => {
                    if self.look(&prog.looks[*k], pos)? {
                        stack.push(pc + 1);
                    }
                }
                Inst::Save(_) | Inst::Mark(_) | Inst::Check(_) => stack.push(pc + 1),
                Inst::Backref(_) | Inst::Require(_) => return Err(E::BadPat),
                Inst::Match => matched = true,
                Inst::Char(_) | Inst::Set(_) => list.dense.push(pc),
            }
        }
        Ok(matched)
    }

    /// Runs `prog` over the text as a set of threads at once: time linear in the text.
    fn pike(&mut self, prog: &Prog, mode: Mode) -> Result<bool, E> {
        let n = self.text.len();
        let (first, last) = match mode {
            Mode::Search | Mode::At(_) => (if let Mode::At(p) = mode { p } else { 0 }, n),
            Mode::EndAt(p) => (0, p),
        };
        let mut clist = Sparse::new(prog.insts.len());
        let mut nlist = Sparse::new(prog.insts.len());
        let mut stack = vec![];
        for p in first..=last {
            let start = match mode {
                Mode::At(q) => p == q,
                _ => true,
            };
            // a thread that started before is in clist (added while the last character was read)
            if start && self.add(prog, &mut clist, 0, p, &mut stack)? {
                match mode {
                    Mode::EndAt(q) if p != q => {}
                    _ => return Ok(true),
                }
            }
            if p == last {
                break;
            }
            let ch = self.text[p];
            nlist.clear();
            let threads = std::mem::take(&mut clist.dense);
            for pc in threads {
                let ok = match &prog.insts[pc] {
                    Inst::Char(c) => *c == ch,
                    Inst::Set(i) => self.set_has(*i, ch),
                    _ => false,
                };
                if ok && self.add(prog, &mut nlist, pc + 1, p + 1, &mut stack)? {
                    match mode {
                        Mode::EndAt(q) if p + 1 != q => {}
                        _ => return Ok(true),
                    }
                }
            }
            std::mem::swap(&mut clist, &mut nlist);
            nlist.clear();
        }
        Ok(false)
    }

    /// Backtracks `prog` from `start`: for a pattern with back-references.
    fn backtrack(&mut self, prog: &Prog, start: usize) -> Result<bool, E> {
        let nslots = 2 * (self.re.groups + 1) + self.re.marks;
        let mark0 = 2 * (self.re.groups + 1);
        let mut slots = vec![NONE; nslots];
        let n = self.text.len();
        let mut stack = vec![Job::Try(0, start)];
        while let Some(job) = stack.pop() {
            let (mut pc, mut pos) = match job {
                Job::Restore(i, old) => {
                    slots[i] = old;
                    continue;
                }
                Job::Try(pc, pos) => (pc, pos),
            };
            loop {
                self.steps += 1;
                if self.steps > MAX_STEPS {
                    return Err(E::TooBig);
                }
                match &prog.insts[pc] {
                    Inst::Char(c) => {
                        if pos < n && self.text[pos] == *c {
                            pc += 1;
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Set(i) => {
                        if pos < n && self.set_has(*i, self.text[pos]) {
                            pc += 1;
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Split(a, b) => {
                        stack.push(Job::Try(*b, pos));
                        pc = *a;
                    }
                    Inst::Jmp(t) => pc = *t,
                    Inst::Assert(a) => {
                        if self.assertion(*a, pos) {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Look(k) => {
                        if self.look(&prog.looks[*k], pos)? {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Save(s) => {
                        stack.push(Job::Restore(*s, slots[*s]));
                        slots[*s] = pos;
                        pc += 1;
                    }
                    Inst::Mark(m) => {
                        stack.push(Job::Restore(mark0 + m, slots[mark0 + m]));
                        slots[mark0 + m] = pos;
                        pc += 1;
                    }
                    Inst::Check(m) => {
                        if slots[mark0 + m] == pos {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Backref(g) => {
                        let (s, e) = (slots[2 * g], slots[2 * g + 1]);
                        if s == NONE || e == NONE || e < s {
                            break;
                        }
                        let len = e - s;
                        if pos + len > n {
                            break;
                        }
                        let same = (0..len).all(|k| {
                            let (a, b) = (self.text[s + k], self.text[pos + k]);
                            a == b || (self.re_icase() && ctype::to_lower(a) == ctype::to_lower(b))
                        });
                        if !same {
                            break;
                        }
                        pos += len;
                        pc += 1;
                    }
                    Inst::Require(g) => {
                        if slots[2 * g] == NONE || slots[2 * g + 1] == NONE {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Match => return Ok(true),
                }
            }
        }
        Ok(false)
    }

    fn re_icase(&self) -> bool {
        self.re.icase
    }
}
