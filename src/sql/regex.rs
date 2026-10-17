//! Regular expressions for `~`, `regexp_replace`, `regexp_match` and friends: Postgres's advanced
//! syntax (`.`, `[a-z]`, `[^...]`, `\d \w \s` and their capitals, `^ $`, `( )`, `(?: )`, `|`,
//! `* + ? {n} {n,} {n,m}` and their lazy forms, `\A \Z \m \M \y \Y`, `\1` back references,
//! `(?= ) (?! ) (?<= ) (?<! )`, the escapes for characters, `[[:alpha:]]` and the other classes,
//! and the embedded options `(?i)`, `(?n)`, `(?x)`, ... at the start), matched by backtracking over
//! characters. A step budget stops patterns that would backtrack forever (54001) rather than hang
//! the query, and a stack budget stops matches that would recurse past the thread's stack (a group
//! repeated thousands of times).

use super::{Result, SqlError, err};

/// What a bracket expression or class escape may hold.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Item {
    Range(char, char),
    Word,
    Digit,
    Space,
    Alpha,
    Alnum,
    Upper,
    Lower,
    Punct,
    Xdigit,
    Blank,
    Cntrl,
    Graph,
    Print,
    /// `\D`, `\W`, `\S`: unlike a negated bracket expression, these take a newline in every mode
    NotDigit,
    NotWord,
    NotSpace,
}

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    Any,
    Class(Vec<Item>, bool), // items, negated
    Start,
    End,
    /// `\A` and `\Z`: the ends of the text, whatever the newline mode
    TextStart,
    TextEnd,
    /// `\y`, `\Y`, `\m`, `\M`: a word's edge, anywhere but an edge, its start, its end
    WordEdge,
    NotWordEdge,
    WordStart,
    WordEnd,
    Group(Box<Node>, Option<usize>), // capture index
    /// `(?=x)`, `(?!x)`, `(?<=x)`, `(?<!x)`: what x does not consume; ahead, negated
    Look(Box<Node>, bool, bool),
    /// `\1`: what the group matched
    Backref(usize),
    /// Nothing, but only where the group has matched: a repeated `\1` needs one
    Matched(usize),
    Alt(Vec<Node>),
    Seq(Vec<Node>),
    Repeat(Box<Node>, usize, Option<usize>, bool), // min, max, greedy
}

pub struct Regex {
    node: Node,
    groups: usize,
    /// whether the match to take is the longest (else the shortest) of those at the same place
    greedy: bool,
    ci: bool,
    /// whether `.` and a negated bracket expression match a newline (not in the `n` and `p` modes)
    dot_nl: bool,
    /// whether `^` and `$` match at the newlines inside the text (in the `n` and `w` modes)
    anchor_nl: bool,
}

pub type Captures = Vec<Option<(usize, usize)>>; // [whole, group 1, ...] as char positions

/// Steps a call (a `~`, or every match of a `regexp_replace`) may take, at least; more for a
/// long text (`STEPS_PER_CHAR` each).
const BUDGET: usize = 10_000_000;
const STEPS_PER_CHAR: usize = 100;
/// Stack a match may use: matching recurses (a frame or so per group repetition), so the text
/// a group repeats over is bounded by this, not by the thread's stack.
const STACK: usize = 512 << 10;
/// Parentheses a pattern may nest: parsing recurses too.
const MAX_NEST: usize = 256;

fn class_of(c: char) -> Option<Item> {
    Some(match c {
        'd' => Item::Digit,
        'D' => Item::NotDigit,
        'w' => Item::Word,
        'W' => Item::NotWord,
        's' => Item::Space,
        'S' => Item::NotSpace,
        _ => return None,
    })
}

/// Whether `c` is in a named class (in the text's locale, so a letter with an accent is a letter).
fn has(item: Item, c: char) -> bool {
    match item {
        Item::Range(lo, hi) => (lo..=hi).contains(&c),
        Item::Word => c.is_alphanumeric() || c == '_',
        Item::Digit => c.is_ascii_digit(),
        Item::Space => c.is_whitespace(),
        Item::Alpha => c.is_alphabetic(),
        Item::Alnum => c.is_alphanumeric(),
        Item::Upper => c.is_uppercase(),
        Item::Lower => c.is_lowercase(),
        Item::Punct => {
            c.is_ascii_punctuation() || (!c.is_ascii() && !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control())
        }
        Item::Xdigit => c.is_ascii_hexdigit(),
        Item::Blank => c == ' ' || c == '\t',
        Item::Cntrl => c.is_control(),
        Item::Graph => !c.is_whitespace() && !c.is_control(),
        Item::Print => !c.is_control(),
        Item::NotDigit => !c.is_ascii_digit(),
        Item::NotWord => !(c.is_alphanumeric() || c == '_'),
        Item::NotSpace => !c.is_whitespace(),
    }
}

struct Parser<'a> {
    p: &'a [char],
    i: usize,
    groups: usize,
    nest: usize,
    /// `x`: whitespace and `#` comments in the pattern are ignored
    expanded: bool,
    /// which groups (by number) have been closed: only those can be referred back to
    closed: Vec<bool>,
    /// how many lookarounds deep the parser is: their parentheses capture nothing
    looking: usize,
}

impl Parser<'_> {
    fn alt(&mut self) -> std::result::Result<Node, String> {
        let mut options = vec![self.seq()?];
        while self.p.get(self.i) == Some(&'|') {
            self.i += 1;
            options.push(self.seq()?);
        }
        Ok(if options.len() == 1 { options.pop().unwrap() } else { Node::Alt(options) })
    }

    fn seq(&mut self) -> std::result::Result<Node, String> {
        let mut items = vec![];
        loop {
            if self.expanded {
                self.skip_blanks();
            }
            let Some(&c) = self.p.get(self.i) else { break };
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            if self.expanded {
                self.skip_blanks();
            }
            items.push(self.quantified(atom)?);
        }
        Ok(Node::Seq(items))
    }

    /// Whitespace, and comments from `#` to the end of the line (the `x` option).
    fn skip_blanks(&mut self) {
        while let Some(&c) = self.p.get(self.i) {
            if c.is_whitespace() {
                self.i += 1;
            } else if c == '#' {
                while self.p.get(self.i).is_some_and(|c| *c != '\n') {
                    self.i += 1;
                }
            } else {
                break;
            }
        }
    }

    fn quantified(&mut self, atom: Node) -> std::result::Result<Node, String> {
        let (min, max) = match self.p.get(self.i) {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            // {n}, {n,}, {n,m}; a { with no digit after it is itself
            Some('{') if self.p.get(self.i + 1).is_some_and(char::is_ascii_digit) => {
                let close = self.p[self.i..].iter().position(|c| *c == '}').ok_or("unmatched {")? + self.i;
                let body: String = self.p[self.i + 1..close].iter().collect();
                let num = |s: &str| s.trim().parse::<usize>().map_err(|_| "invalid repetition count(s)".to_string());
                let (min, max) = match body.split_once(',') {
                    None => {
                        let n = num(&body)?;
                        (n, Some(n))
                    }
                    Some((a, b)) if b.trim().is_empty() => (num(a)?, None),
                    Some((a, b)) => (num(a)?, Some(num(b)?)),
                };
                if max.is_some_and(|m| m < min) || min > 255 || max.is_some_and(|m| m > 255) {
                    return Err("invalid repetition count(s)".into());
                }
                self.i = close; // the } is stepped over below
                (min, max)
            }
            _ => return Ok(atom),
        };
        // an anchor or a lookaround is a place, not a thing to repeat
        if matches!(
            atom,
            Node::Start
                | Node::End
                | Node::TextStart
                | Node::TextEnd
                | Node::WordEdge
                | Node::NotWordEdge
                | Node::WordStart
                | Node::WordEnd
                | Node::Look(..)
        ) {
            return Err("quantifier operand invalid".into());
        }
        self.i += 1;
        let greedy = if self.p.get(self.i) == Some(&'?') {
            self.i += 1;
            false
        } else {
            true
        };
        // `\1?` and `\1*` fail, unlike `(\1)?`, when their group has not matched (`\1{0}` cannot)
        let bound = match atom {
            Node::Backref(g) if max != Some(0) => Some(g),
            _ => None,
        };
        let repeat = Node::Repeat(Box::new(atom), min, max, greedy);
        Ok(match bound {
            Some(g) => Node::Seq(vec![Node::Matched(g), repeat]),
            None => repeat,
        })
    }

    fn atom(&mut self) -> std::result::Result<Node, String> {
        let c = self.p[self.i];
        self.i += 1;
        Ok(match c {
            '.' => Node::Any,
            '^' => Node::Start,
            '$' => Node::End,
            '(' => {
                if self.nest >= MAX_NEST {
                    return Err(format!("parentheses nested more than {MAX_NEST} deep"));
                }
                // (?: non-capturing, (?= (?! (?<= (?<! looking around
                let mut look = None;
                let capture = if self.p.get(self.i) == Some(&'?') {
                    let (ahead, neg, skip) = match (self.p.get(self.i + 1), self.p.get(self.i + 2)) {
                        (Some(':'), _) => (true, false, 2),
                        (Some('='), _) => (true, false, 2),
                        (Some('!'), _) => (true, true, 2),
                        (Some('<'), Some('=')) => (false, false, 3),
                        (Some('<'), Some('!')) => (false, true, 3),
                        _ => return Err("invalid regular expression: quantifier operand invalid".into()),
                    };
                    if self.p[self.i + 1] != ':' {
                        look = Some((ahead, neg));
                    }
                    self.i += skip;
                    None
                } else if self.looking > 0 {
                    None
                } else {
                    self.groups += 1;
                    self.closed.push(false);
                    Some(self.groups)
                };
                self.nest += 1;
                self.looking += usize::from(look.is_some());
                let inner = self.alt()?;
                self.looking -= usize::from(look.is_some());
                self.nest -= 1;
                if self.p.get(self.i) != Some(&')') {
                    return Err("parentheses () not balanced".into());
                }
                self.i += 1;
                if let Some(g) = capture {
                    self.closed[g] = true;
                }
                match look {
                    Some((ahead, neg)) => Node::Look(Box::new(inner), ahead, neg),
                    None => Node::Group(Box::new(inner), capture),
                }
            }
            '[' => {
                // [[:<:]] and [[:>:]]: the start and the end of a word
                if self.p[self.i..].starts_with(&['[', ':', '<', ':', ']', ']']) {
                    self.i += 6;
                    Node::WordStart
                } else if self.p[self.i..].starts_with(&['[', ':', '>', ':', ']', ']']) {
                    self.i += 6;
                    Node::WordEnd
                } else {
                    self.class()?
                }
            }
            '\\' => self.escape()?,
            '*' | '+' | '?' => return Err(format!("quantifier operand invalid: {c} follows nothing")),
            '{' if self.p.get(self.i).is_some_and(char::is_ascii_digit) => {
                return Err("quantifier operand invalid".into());
            }
            c => Node::Char(c),
        })
    }

    /// After a backslash outside a bracket expression.
    fn escape(&mut self) -> std::result::Result<Node, String> {
        let e = *self.p.get(self.i).ok_or("invalid escape \\ sequence")?;
        self.i += 1;
        if let Some(item) = class_of(e) {
            return Ok(Node::Class(vec![item], false));
        }
        Ok(match e {
            'A' => Node::TextStart,
            'Z' => Node::TextEnd,
            'y' => Node::WordEdge,
            'Y' => Node::NotWordEdge,
            'm' => Node::WordStart,
            'M' => Node::WordEnd,
            '1'..='9' => {
                // a back reference: a lone digit always, more digits only if that many groups
                // have begun; else an octal number
                let start = self.i - 1;
                let mut end = self.i;
                let mut n = e.to_digit(10).unwrap_or(0) as usize;
                while let Some(d) = self.p.get(end).and_then(|c| c.to_digit(10)) {
                    n = n
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(d as usize))
                        .ok_or("invalid escape \\ sequence")?;
                    end += 1;
                }
                if end == start + 1 || n <= self.groups {
                    // no group to refer to yet, none like it, or a lookaround, which may not
                    if self.looking > 0 || !self.closed.get(n).copied().unwrap_or(false) {
                        return Err("invalid backreference number".into());
                    }
                    self.i = end;
                    Node::Backref(n)
                } else {
                    self.i = start;
                    Node::Char(self.octal()?)
                }
            }
            '0' => {
                self.i -= 1;
                Node::Char(self.octal()?)
            }
            _ => {
                self.i -= 1;
                Node::Char(self.char_escape()?)
            }
        })
    }

    /// Up to three octal digits.
    fn octal(&mut self) -> std::result::Result<char, String> {
        let mut v = 0u32;
        let mut n = 0;
        while n < 3
            && let Some(d) = self.p.get(self.i).and_then(|c| c.to_digit(8))
        {
            v = v * 8 + d;
            self.i += 1;
            n += 1;
        }
        if n == 0 {
            return Err("invalid escape \\ sequence".into());
        }
        char::from_u32(v).ok_or_else(|| "invalid escape \\ sequence".into())
    }

    /// The escape of one character, after its backslash (`self.i` at the letter): `\n`, `\t`,
    /// `\e`, `\cX`, `\x41`, `\u0041`, `\U00000041`, or any punctuation for itself.
    fn char_escape(&mut self) -> std::result::Result<char, String> {
        let bad = || "invalid escape \\ sequence".to_string();
        let e = *self.p.get(self.i).ok_or_else(bad)?;
        self.i += 1;
        let hex = |this: &mut Self, max: usize| -> std::result::Result<char, String> {
            let mut v = 0u32;
            let mut n = 0;
            while n < max
                && let Some(d) = this.p.get(this.i).and_then(|c| c.to_digit(16))
            {
                v = v.saturating_mul(16).saturating_add(d);
                this.i += 1;
                n += 1;
            }
            if n == 0 || (max != usize::MAX && n < max) {
                return Err("invalid escape \\ sequence".into());
            }
            char::from_u32(v).ok_or_else(|| "invalid escape \\ sequence".into())
        };
        Ok(match e {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'f' => '\u{c}',
            'v' => '\u{b}',
            'a' => '\u{7}',
            'b' => '\u{8}',
            'B' => '\\',
            'e' => '\u{1b}',
            'c' => {
                let x = *self.p.get(self.i).ok_or_else(bad)?;
                self.i += 1;
                char::from_u32(x as u32 & 0x1f).ok_or_else(bad)?
            }
            'x' => hex(self, usize::MAX)?,
            'u' => hex(self, 4)?,
            'U' => hex(self, 8)?,
            c if c.is_alphanumeric() => return Err(bad()),
            c => c,
        })
    }

    fn class(&mut self) -> std::result::Result<Node, String> {
        let neg = self.p.get(self.i) == Some(&'^');
        if neg {
            self.i += 1;
        }
        let mut items = vec![];
        let mut first = true;
        loop {
            let c = *self.p.get(self.i).ok_or("brackets [] not balanced")?;
            self.i += 1;
            match c {
                ']' if !first => break,
                '[' if self.p.get(self.i) == Some(&':') => {
                    // [:alpha:] and friends
                    let from = self.i + 1;
                    let end =
                        self.p[from..].windows(2).position(|w| w == [':', ']']).ok_or("brackets [] not balanced")?
                            + from;
                    let name: String = self.p[from..end].iter().collect();
                    self.i = end + 2;
                    items.push(match name.as_str() {
                        "alpha" => Item::Alpha,
                        "digit" => Item::Digit,
                        "alnum" => Item::Alnum,
                        "space" => Item::Space,
                        "upper" => Item::Upper,
                        "lower" => Item::Lower,
                        "punct" => Item::Punct,
                        "xdigit" => Item::Xdigit,
                        "blank" => Item::Blank,
                        "cntrl" => Item::Cntrl,
                        "graph" => Item::Graph,
                        "print" => Item::Print,
                        "word" => Item::Word,
                        _ => return Err("invalid character class".into()),
                    });
                }
                '[' if matches!(self.p.get(self.i), Some('.' | '=')) => {
                    // [.a.] a collating element, [=a=] an equivalence class: the one character
                    let kind = self.p[self.i];
                    let from = self.i + 1;
                    let end =
                        self.p[from..].windows(2).position(|w| w == [kind, ']']).ok_or("brackets [] not balanced")?
                            + from;
                    let name: String = self.p[from..end].iter().collect();
                    self.i = end + 2;
                    let mut it = name.chars();
                    match (it.next(), it.next()) {
                        (Some(c), None) => items.push(Item::Range(c, c)),
                        _ => return Err("invalid collating element".into()),
                    }
                }
                '\\' => {
                    let e = *self.p.get(self.i).ok_or("invalid escape \\ sequence")?;
                    match class_of(e) {
                        Some(item) => {
                            self.i += 1;
                            items.push(item);
                        }
                        None => {
                            let c = if e.is_ascii_digit() { self.octal()? } else { self.char_escape()? };
                            items.push(Item::Range(c, c));
                        }
                    }
                }
                c => {
                    if self.p.get(self.i) == Some(&'-') && self.p.get(self.i + 1).is_some_and(|x| *x != ']') {
                        let hi = self.p[self.i + 1];
                        self.i += 2;
                        if hi < c {
                            return Err("invalid character range".into());
                        }
                        items.push(Item::Range(c, hi));
                    } else {
                        items.push(Item::Range(c, c));
                    }
                }
            }
            first = false;
        }
        Ok(Node::Class(items, neg))
    }
}

/// The options a regular expression is compiled with, from a call's flag letters and from an
/// `(?...)` at the pattern's start.
#[derive(Clone, Copy)]
struct Opts {
    ci: bool,
    dot_nl: bool,
    anchor_nl: bool,
    expanded: bool,
    literal: bool,
}

impl Opts {
    /// The letters `b c e i m n p q s t w x` (Postgres's, in order: a later one overrides an earlier).
    fn apply(&mut self, letters: &str) -> std::result::Result<(), char> {
        for l in letters.chars() {
            match l {
                'i' => self.ci = true,
                'c' => self.ci = false,
                // newline-sensitive: . and [^] stop at a newline, ^ and $ match at one
                'n' | 'm' => (self.dot_nl, self.anchor_nl) = (false, true),
                // partial: . and [^] stop at a newline
                'p' => (self.dot_nl, self.anchor_nl) = (false, false),
                // inverse partial: ^ and $ match at a newline
                'w' => (self.dot_nl, self.anchor_nl) = (true, true),
                's' => (self.dot_nl, self.anchor_nl) = (true, false),
                'x' => self.expanded = true,
                't' => self.expanded = false,
                'q' => self.literal = true,
                'b' | 'e' => {}
                c => return Err(c),
            }
        }
        Ok(())
    }
}

/// How Postgres decides whether an expression takes its longest or its shortest match: by its
/// first quantified atom (a group has its contents'), or longest if it has alternatives.
fn greediness(n: &Node) -> Option<bool> {
    match n {
        Node::Alt(_) => Some(true),
        Node::Seq(items) => items.iter().find_map(greediness),
        // {m} takes its atom's; the others their own
        Node::Repeat(inner, min, Some(max), greedy) if min == max && *greedy => greediness(inner),
        Node::Repeat(_, _, _, greedy) => Some(*greedy),
        Node::Group(inner, _) => greediness(inner),
        _ => None,
    }
}

impl Regex {
    #[cfg(test)]
    pub fn new(pattern: &str, ci: bool) -> std::result::Result<Regex, String> {
        Regex::with_flags(pattern, if ci { "i" } else { "" })
    }

    /// Compiled with option letters (`i`, `n`, ...) as well as those the pattern starts with.
    pub fn with_flags(pattern: &str, flags: &str) -> std::result::Result<Regex, String> {
        if pattern.len() > MAX_PATTERN {
            return Err("regular expression is too large".into());
        }
        let mut o = Opts { ci: false, dot_nl: true, anchor_nl: false, expanded: false, literal: false };
        o.apply(flags).map_err(|c| format!("invalid regular expression option: \"{c}\""))?;
        let mut chars: Vec<char> = pattern.chars().collect();
        // `***=` says the rest is literal; `***:` that it is an advanced expression
        if chars.starts_with(&['*', '*', '*', '=']) {
            chars.drain(..4);
            o.literal = true;
        } else if chars.starts_with(&['*', '*', '*', ':']) {
            chars.drain(..4);
        }
        // embedded options, only at the start: (?i), (?nx), ...
        if !o.literal
            && chars.starts_with(&['(', '?'])
            && let Some(close) = chars.iter().position(|c| *c == ')')
            && close > 2
            && chars[2..close].iter().all(|c| "bceimnpqstwx".contains(*c))
        {
            let letters: String = chars[2..close].iter().collect();
            o.apply(&letters).map_err(|c| format!("invalid regular expression option: \"{c}\""))?;
            chars.drain(..=close);
        }
        let (node, groups) = if o.literal {
            (Node::Seq(chars.iter().map(|c| Node::Char(*c)).collect()), 0)
        } else {
            let mut p =
                Parser { p: &chars, i: 0, groups: 0, nest: 0, expanded: o.expanded, closed: vec![false], looking: 0 };
            let node = p.alt()?;
            if p.i < chars.len() {
                return Err("parentheses () not balanced".into());
            }
            (node, p.groups)
        };
        let greedy = greediness(&node).unwrap_or(true);
        Ok(Regex { node, groups, greedy, ci: o.ci, dot_nl: o.dot_nl, anchor_nl: o.anchor_nl })
    }

    /// The match at or after character `from` that starts earliest and, of those, is the longest
    /// (the shortest for an expression that is not greedy), as Postgres has it: the whole match and
    /// each group. A search from 0 starts a call's step budget; the searches after it (`from` > 0,
    /// for the next matches) share it.
    pub fn find_at(&self, s: &[char], from: usize) -> Result<Option<Captures>> {
        self.find(s, from, true)
    }

    fn find(&self, s: &[char], from: usize, longest: bool) -> Result<Option<Captures>> {
        if from == 0 {
            STEPS.set(BUDGET.saturating_add(s.len().saturating_mul(STEPS_PER_CHAR)));
        }
        let here = 0u8;
        BASE.set(std::ptr::addr_of!(here) as usize);
        for start in from..=s.len() {
            let mut caps = vec![None; self.groups + 1];
            let mut best: Option<(usize, Captures)> = None;
            self.m(&self.node, s, start, &mut caps, &mut |e, c| {
                let better = best.as_ref().is_none_or(|(b, _)| if self.greedy { e > *b } else { e < *b });
                if better {
                    best = Some((e, c.clone()));
                }
                // a match is enough where only whether there is one matters, or no other could be better
                !longest || (self.greedy && e == s.len()) || (!self.greedy && e == start)
            })?;
            if let Some((e, mut caps)) = best {
                caps[0] = Some((start, e));
                return Ok(Some(caps));
            }
        }
        Ok(None)
    }

    pub fn is_match(&self, s: &str) -> Result<bool> {
        let chars = super::funcs::chars(s)?;
        Ok(self.find(&chars, 0, false)?.is_some())
    }

    fn eq(&self, a: char, b: char) -> bool {
        a == b || (self.ci && a.to_lowercase().eq(b.to_lowercase()))
    }

    fn in_class(&self, c: char, items: &[Item]) -> bool {
        let hit = |c: char| items.iter().any(|i| has(*i, c));
        hit(c)
            || (self.ci
                && (c.to_lowercase().any(hit)
                    || c.to_uppercase().any(hit)
                    || (c.is_alphabetic() && items.iter().any(|i| matches!(i, Item::Upper | Item::Lower)))))
    }

    /// Whether a class node, negated or not, takes `c` (a negated one takes no newline in the
    /// newline-sensitive modes).
    fn takes(&self, c: char, items: &[Item], neg: bool) -> bool {
        if neg { !self.in_class(c, items) && (self.dot_nl || c != '\n') } else { self.in_class(c, items) }
    }

    /// Matches `n` at `i`, then calls `k` with where it ended; true once `k` accepts.
    fn m(
        &self,
        n: &Node,
        s: &[char],
        i: usize,
        caps: &mut Captures,
        k: &mut dyn FnMut(usize, &mut Captures) -> bool,
    ) -> Result<bool> {
        step(1)?;
        let here = 0u8;
        if BASE.get().abs_diff(std::ptr::addr_of!(here) as usize) > STACK {
            return err("54001", "regular expression is too complex to match: it recurses too deeply over this text");
        }
        let word = |j: usize| s.get(j).is_some_and(|c| c.is_alphanumeric() || *c == '_');
        let before = |j: usize| j > 0 && word(j - 1);
        let one = |j: usize, n: &Node| match (s.get(j), n) {
            (Some(c), Node::Char(x)) => self.eq(*c, *x),
            (Some(c), Node::Any) => self.dot_nl || *c != '\n',
            (Some(c), Node::Class(r, neg)) => self.takes(*c, r, *neg),
            _ => false,
        };
        Ok(match n {
            Node::Char(c) => s.get(i).is_some_and(|x| self.eq(*x, *c)) && k(i + 1, caps),
            Node::Any => s.get(i).is_some_and(|c| self.dot_nl || *c != '\n') && k(i + 1, caps),
            Node::Class(r, neg) => s.get(i).is_some_and(|c| self.takes(*c, r, *neg)) && k(i + 1, caps),
            Node::Start => (i == 0 || (self.anchor_nl && s[i - 1] == '\n')) && k(i, caps),
            Node::End => (i == s.len() || (self.anchor_nl && s[i] == '\n')) && k(i, caps),
            Node::TextStart => i == 0 && k(i, caps),
            Node::TextEnd => i == s.len() && k(i, caps),
            Node::WordEdge => before(i) != word(i) && k(i, caps),
            Node::NotWordEdge => before(i) == word(i) && k(i, caps),
            Node::WordStart => !before(i) && word(i) && k(i, caps),
            Node::WordEnd => before(i) && !word(i) && k(i, caps),
            Node::Backref(g) => match caps[*g] {
                Some((a, b)) => {
                    let len = b - a;
                    i + len <= s.len() && (0..len).all(|d| self.eq(s[i + d], s[a + d])) && k(i + len, caps)
                }
                None => false,
            },
            Node::Matched(g) => caps[*g].is_some() && k(i, caps),
            Node::Look(inner, ahead, neg) => {
                let mut scratch = caps.clone();
                let found = if *ahead {
                    self.m(inner, s, i, &mut scratch, &mut |_, _| true)?
                } else {
                    let mut found = false;
                    for j in (0..=i).rev() {
                        if self.m(inner, s, j, &mut scratch, &mut |e, _| e == i)? {
                            found = true;
                            break;
                        }
                    }
                    found
                };
                found != *neg && k(i, caps)
            }
            Node::Group(inner, cap) => {
                let before = cap.map(|g| caps[g]);
                let mut hit = false;
                let res = self.m(inner, s, i, caps, &mut |e, caps: &mut Captures| {
                    if let Some(g) = cap {
                        caps[*g] = Some((i, e));
                    }
                    hit = k(e, caps);
                    hit
                })?;
                if !res && let (Some(g), Some(b)) = (cap, before) {
                    caps[*g] = b;
                }
                res
            }
            Node::Alt(options) => {
                for o in options {
                    if self.m(o, s, i, caps, k)? {
                        return Ok(true);
                    }
                }
                false
            }
            Node::Seq(items) => self.seq(items, s, i, caps, k)?,
            Node::Repeat(inner, min, max, greedy) if matches!(**inner, Node::Char(_) | Node::Any | Node::Class(..)) => {
                // one character each time: as far as it goes, then the rest from each place it
                // could stop (the longest first, if greedy), with no frame per character
                let mut n = 0;
                while max.is_none_or(|m| n < m) && one(i + n, inner) {
                    n += 1;
                }
                step(n)?;
                if n < *min {
                    return Ok(false);
                }
                for j in *min..=n {
                    step(1)?;
                    if k(i + if *greedy { n + *min - j } else { j }, caps) {
                        return Ok(true);
                    }
                }
                false
            }
            Node::Repeat(inner, min, max, greedy) => self.rep(inner, *min, *max, *greedy, 0, s, i, caps, k)?,
        })
    }

    fn seq(
        &self,
        items: &[Node],
        s: &[char],
        i: usize,
        caps: &mut Captures,
        k: &mut dyn FnMut(usize, &mut Captures) -> bool,
    ) -> Result<bool> {
        match items.split_first() {
            None => Ok(k(i, caps)),
            Some((first, rest)) => {
                let mut err = None;
                let r =
                    self.m(first, s, i, caps, &mut |e, caps: &mut Captures| match self.seq(rest, s, e, caps, k) {
                        Ok(b) => b,
                        Err(x) => {
                            err = Some(x);
                            true
                        }
                    })?;
                if let Some(e) = err {
                    return Err(e);
                }
                Ok(r)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rep(
        &self,
        inner: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        done: usize,
        s: &[char],
        i: usize,
        caps: &mut Captures,
        k: &mut dyn FnMut(usize, &mut Captures) -> bool,
    ) -> Result<bool> {
        // the order of the two tries is the point: greedy takes one more first, lazy stops first
        #[allow(clippy::if_same_then_else)]
        let r = if done < min {
            self.more(inner, min, max, greedy, done, s, i, caps, k)?
        } else if greedy {
            self.more(inner, min, max, greedy, done, s, i, caps, k)? || k(i, caps)
        } else {
            k(i, caps) || self.more(inner, min, max, greedy, done, s, i, caps, k)?
        };
        Ok(r)
    }

    /// One more repetition of `inner` at `i`, then the rest of `rep`.
    #[allow(clippy::too_many_arguments)]
    fn more(
        &self,
        inner: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        done: usize,
        s: &[char],
        i: usize,
        caps: &mut Captures,
        k: &mut dyn FnMut(usize, &mut Captures) -> bool,
    ) -> Result<bool> {
        if max.is_some_and(|m| done >= m) {
            return Ok(false);
        }
        let mut err = None;
        let r = self.m(inner, s, i, caps, &mut |e, caps: &mut Captures| {
            if e == i && done >= min.max(1) {
                return false; // an empty match repeated gets nowhere (but the first one counts)
            }
            match self.rep(inner, min, max, greedy, done + 1, s, e, caps, k) {
                Ok(b) => b,
                Err(x) => {
                    err = Some(x);
                    true
                }
            }
        })?;
        if let Some(e) = err {
            return Err(e);
        }
        Ok(r)
    }
}

/// The longest pattern: Postgres refuses to compile about as large.
const MAX_PATTERN: usize = 1 << 20;

// The steps a call has left, and where on the stack its search began: per thread, as matching
// nests closures in closures.
thread_local! {
    static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(BUDGET) };
    static BASE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Takes `n` steps from the budget: an error once it's spent, or when the statement has to stop
/// (checked every 4096 steps, as `tick` does).
fn step(n: usize) -> Result<()> {
    let left = STEPS.get();
    if left < n.max(1) {
        return Err(SqlError {
            code: "54001",
            message: "regular expression is too complex to match".into(),
            info: None,
        });
    }
    STEPS.set(left - n);
    if left / 4096 != (left - n) / 4096 {
        super::stopped()?;
    }
    Ok(())
}

/// `regexp_replace`: `repl` with `\1`..`\9` for groups and `\&` for the whole match; every match
/// with `global`, else the first.
pub fn replace(re: &Regex, s: &str, repl: &str, global: bool) -> Result<String> {
    let chars = super::funcs::chars(s)?;
    let (mut out, mut at) = (String::new(), 0);
    while at <= chars.len() {
        let Some(caps) = re.find_at(&chars, at)? else { break };
        let (ms, me) = caps[0].unwrap();
        out.extend(&chars[at..ms]);
        expand(&mut out, &chars, &caps, repl)?;
        if me == ms {
            // an empty match: copy one character on, so the next search moves
            out.extend(chars.get(ms));
            at = me + 1;
        } else {
            at = me;
        }
        if !global {
            out.extend(chars.get(at..).unwrap_or_default());
            return Ok(out);
        }
    }
    out.extend(chars.get(at..).unwrap_or_default());
    Ok(out)
}

/// A replacement text with its `\1`..`\9` and `\&` filled in from a match, added to `out`.
pub fn expand(out: &mut String, chars: &[char], caps: &Captures, repl: &str) -> Result<()> {
    let (ms, me) = caps[0].unwrap();
    let mut r = repl.chars().peekable();
    while let Some(c) = r.next() {
        // refused as it grows: each \& or \1 can copy a whole match again
        super::funcs::fits(Some(out.len()))?;
        match (c, r.peek().copied()) {
            ('\\', Some(d)) if d.is_ascii_digit() => {
                r.next();
                if let Some(Some((a, b))) = caps.get(d as usize - '0' as usize) {
                    out.extend(&chars[*a..*b]);
                }
            }
            ('\\', Some('&')) => {
                r.next();
                out.extend(&chars[ms..me]);
            }
            ('\\', Some('\\')) => {
                r.next();
                out.push('\\');
            }
            (c, _) => out.push(c),
        }
    }
    Ok(())
}

/// `regexp_match`: the groups of the first match (or the whole match without groups).
pub fn first_match(re: &Regex, s: &str) -> Result<Option<Vec<Option<String>>>> {
    let chars = super::funcs::chars(s)?;
    let Some(caps) = re.find_at(&chars, 0)? else { return Ok(None) };
    let text = |c: &Option<(usize, usize)>| c.map(|(a, b)| chars[a..b].iter().collect());
    Ok(Some(if caps.len() == 1 { vec![text(&caps[0])] } else { caps[1..].iter().map(text).collect() }))
}

/// `regexp_split_to_array`: the pieces between matches.
pub fn split(re: &Regex, s: &str) -> Result<Vec<String>> {
    use super::funcs::{ITEM, fits};
    let chars = super::funcs::chars(s)?;
    let (mut out, mut at, mut from, mut held) = (vec![], 0, 0, 0usize);
    while from <= chars.len() {
        let Some(caps) = re.find_at(&chars, from)? else { break };
        let (ms, me) = caps[0].unwrap();
        if me == ms {
            if ms >= chars.len() {
                break;
            }
            if ms == at {
                from = ms + 1;
                continue;
            }
        }
        let piece: String = chars[at..ms].iter().collect();
        held = fits(held.checked_add(ITEM + piece.len()))?; // an item costs far more than its text
        out.push(piece);
        at = me;
        from = if me == ms { me + 1 } else { me };
    }
    out.push(chars[at..].iter().collect());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        Regex::new(p, false).unwrap().is_match(s).unwrap()
    }

    #[test]
    fn matches_like_postgres() {
        assert!(m("^a.c$", "abc") && !m("^a.c$", "abbc"));
        assert!(m("[0-9]{3}-[0-9]{4}", "call 555-1234 now"));
        assert!(m("^(foo|bar)+$", "foobarfoo") && !m("^(foo|bar)+$", "foobaz"));
        assert!(m("\\d+\\.\\d+", "v1.25") && m("^\\w+@\\w+\\.com$", "ada@example.com"));
        assert!(m("colou?r", "color") && m("colou?r", "colour"));
        assert!(m("[[:alpha:]]+", "abc") && !m("^[^a-z]+$", "abc"));
        assert!(Regex::new("ABC", true).unwrap().is_match("xabcx").unwrap());
        // (\y is the edge of a word; \b is a backspace, as in Postgres)
        assert!(m("\\ycat\\y", "a cat!") && !m("\\ycat\\y", "concat") && !m("\\bcat", "a cat"));
        let re = Regex::new("(\\w+)@(\\w+)", false).unwrap();
        assert_eq!(replace(&re, "ada@x and bo@y", "\\2:\\1", true).unwrap(), "x:ada and y:bo");
        assert_eq!(replace(&re, "ada@x and bo@y", "<\\&>", false).unwrap(), "<ada@x> and bo@y");
        assert_eq!(first_match(&re, "hi ada@x").unwrap(), Some(vec![Some("ada".into()), Some("x".into())]));
        assert_eq!(split(&Regex::new("\\s*,\\s*", false).unwrap(), "a , b,c").unwrap(), ["a", "b", "c"]);
        assert_eq!(replace(&Regex::new("a*?", false).unwrap(), "baaa", "-", true).unwrap(), "-b-a-a-a-");
        // catastrophic backtracking is stopped, not waited out
        assert!(Regex::new("^(a+)+$", false).unwrap().is_match(&("a".repeat(40) + "b")).is_err());
        assert!(Regex::new("(abc", false).is_err() && Regex::new("*a", false).is_err());
    }
}
