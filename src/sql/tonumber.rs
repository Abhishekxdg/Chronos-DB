//! `to_number(text, template)`: reading a number out of text by a template, as Postgres's
//! `numeric_to_number` does (`parse_format` and `NUMDesc_prepare` for the template, `NUM_processor`
//! and `NUM_numpart_from_char` for the text, then `numeric_in` with the precision and scale the
//! reading implies, and a multiplication for `V`).
//!
//! The reading is not a parse, and Postgres's oddities are kept, because they show in answers:
//! every pattern takes one character of the text whether it fits or not (`9` and `0` read a digit
//! if there is one, and a blank before it, a sign before the first, a sign after the last); a
//! character in the template that is no pattern (or is quoted) skips one character of the text,
//! whatever it is; `,` and `G` wait for a comma and, under `FM`, never take one; `L` skips a
//! character that is no part of a number, and `TH` up to two (but none when the template has a
//! decimal point); text left over is ignored, and text that runs out ends the reading. More digits
//! than the template has are not read. `RN` and `EEEE` are for `to_char` only (0A000).
//!
//! Places come from the C locale, as the server's do here: the decimal point is `.`, the group
//! separator `,`, the signs `-` and `+`, and the currency symbol is one blank, so `L` skips one
//! character (`D`, `G` and `S` are read like `.`, `,` and a sign).
//!
//! The text is `numeric_in`'d with a type of the digits the template has before the point, plus
//! those of `V`, plus the ones after it that the text had: so the answer keeps the scale the text
//! had (`to_number('12.50', '99.99')` is `12.50`, `to_number('12', '99.99')` is `12`), too many
//! integer digits (which happens when `.` or `D` meets a digit) is 22003, and `V` multiplies by
//! `10^-n`, which Postgres does with a scale of 16 more than `n` (up to 1000).
//!
//! A template so wide that the precision has its top bit set (32768 to 65535 digits, and again from
//! 98304) makes a type that is not applied (no rounding, no range), and then only what a numeric
//! can hold at all stops the value: more than 16383 places, or more than 131072 digits before the
//! point, is 22003 "value overflows numeric format" (`make_result`).

/// Why the text doesn't fit: the SQLSTATE and message.
type Fail = (&'static str, String);

fn fail<T>(code: &'static str, message: impl Into<String>) -> Result<T, Fail> {
    Err((code, message.into()))
}

const SYNTAX: &str = "42601"; // syntax_error: a template that makes no sense
const INVALID: &str = "22P02"; // invalid_text_representation
const RANGE: &str = "22003"; // numeric_value_out_of_range
const UNSUPPORTED: &str = "0A000"; // feature_not_supported

/// The most places after the point a numeric can show (`NUMERIC_DSCALE_MAX`: 14 bits).
const MAX_DSCALE: i64 = 16383;
/// The most digits a numeric can have before the point: its weight is an `int16` of base-10000
/// digits, so 32768 of them, which is 131072 decimal digits (`make_result`).
const MAX_WHOLE_DIGITS: i64 = 131072;

/// What a pattern of the template is.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Key {
    Comma,
    Point,
    Zero,
    Nine,
    Blank,
    Currency,
    Decimal,
    Exponent,
    Fill,
    Group,
    Local,
    Minus,
    Plus,
    Bracket,
    Roman,
    Sign,
    Spell,
    LocalSign,
    Th,
    Shift,
}

/// A piece of the template.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Node {
    Key(Key),
    /// anything else, or quoted text: it skips one character of the text
    Skip,
}

/// The patterns, in the order Postgres tries them: `SG` and `SP` before `S`.
const PATTERNS: &[(&str, Key)] = &[
    (",", Key::Comma),
    (".", Key::Point),
    ("0", Key::Zero),
    ("9", Key::Nine),
    ("B", Key::Blank),
    ("C", Key::Currency),
    ("D", Key::Decimal),
    ("EEEE", Key::Exponent),
    ("FM", Key::Fill),
    ("G", Key::Group),
    ("L", Key::Local),
    ("MI", Key::Minus),
    ("PL", Key::Plus),
    ("PR", Key::Bracket),
    ("RN", Key::Roman),
    ("SG", Key::Sign),
    ("SP", Key::Spell),
    ("S", Key::LocalSign),
    ("TH", Key::Th),
    ("V", Key::Shift),
    ("b", Key::Blank),
    ("c", Key::Currency),
    ("d", Key::Decimal),
    ("eeee", Key::Exponent),
    ("fm", Key::Fill),
    ("g", Key::Group),
    ("l", Key::Local),
    ("mi", Key::Minus),
    ("pl", Key::Plus),
    ("pr", Key::Bracket),
    ("rn", Key::Roman),
    ("sg", Key::Sign),
    ("sp", Key::Spell),
    ("s", Key::LocalSign),
    ("th", Key::Th),
    ("v", Key::Shift),
];

/// What the template says as a whole (Postgres's `NUMDesc`).
#[derive(Default, Debug)]
struct Desc {
    /// digits before the point, and after it, and after `V`
    pre: usize,
    post: usize,
    multi: usize,
    /// a `.` or `D`
    decimal: bool,
    /// a `0`
    zero: bool,
    blank: bool,
    fill: bool,
    /// an `S`
    sign: bool,
    /// a `PR`
    bracket: bool,
    minus: bool,
    plus: bool,
    roman: bool,
    /// a `V`
    shift: bool,
    exponent: bool,
}

fn syntax<T>(message: &str) -> Result<T, Fail> {
    fail(SYNTAX, message)
}

impl Desc {
    /// Takes one more pattern into account, or refuses it (`NUMDesc_prepare`).
    fn add(&mut self, key: Key) -> Result<(), Fail> {
        if self.exponent && key != Key::Exponent {
            return syntax("\"EEEE\" must be the last pattern used");
        }
        match key {
            Key::Nine => {
                if self.bracket {
                    return syntax("\"9\" must be ahead of \"PR\"");
                }
                if self.shift {
                    self.multi += 1;
                } else if self.decimal {
                    self.post += 1;
                } else {
                    self.pre += 1;
                }
            }
            Key::Zero => {
                if self.bracket {
                    return syntax("\"0\" must be ahead of \"PR\"");
                }
                self.zero |= !self.decimal;
                if self.decimal {
                    self.post += 1;
                } else {
                    self.pre += 1;
                }
            }
            Key::Blank => self.blank |= self.pre == 0 && self.post == 0 && !self.zero,
            Key::Decimal | Key::Point => {
                if self.decimal {
                    return syntax("multiple decimal points");
                }
                if self.shift {
                    return syntax("cannot use \"V\" and decimal point together");
                }
                self.decimal = true;
            }
            Key::Fill => self.fill = true,
            Key::LocalSign => {
                if self.sign {
                    return syntax("cannot use \"S\" twice");
                }
                if self.plus || self.minus || self.bracket {
                    return syntax("cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together");
                }
                self.sign = true;
            }
            Key::Minus | Key::Plus | Key::Sign => {
                if self.sign {
                    let name = match key {
                        Key::Minus => "MI",
                        Key::Plus => "PL",
                        _ => "SG",
                    };
                    return syntax(&format!("cannot use \"S\" and \"{name}\" together"));
                }
                self.minus |= key != Key::Plus;
                self.plus |= key != Key::Minus;
            }
            Key::Bracket => {
                if self.sign || self.plus || self.minus {
                    return syntax("cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together");
                }
                self.bracket = true;
            }
            Key::Roman => self.roman = true,
            Key::Shift => {
                if self.decimal {
                    return syntax("cannot use \"V\" and decimal point together");
                }
                self.shift = true;
            }
            Key::Exponent => {
                if self.exponent {
                    return syntax("cannot use \"EEEE\" twice");
                }
                if self.blank
                    || self.fill
                    || self.sign
                    || self.bracket
                    || self.minus
                    || self.plus
                    || self.roman
                    || self.shift
                {
                    return syntax(
                        "\"EEEE\" is incompatible with other formats. \"EEEE\" may only be used together with digit and decimal point patterns.",
                    );
                }
                self.exponent = true;
            }
            Key::Comma | Key::Currency | Key::Group | Key::Local | Key::Spell | Key::Th => {}
        }
        Ok(())
    }
}

/// The template as nodes and what it says as a whole (`parse_format`).
fn parse(fmt: &str) -> Result<(Vec<Node>, Desc), Fail> {
    let mut nodes = vec![];
    let mut desc = Desc::default();
    let mut rest = fmt;
    while let Some(c) = rest.chars().next() {
        if let Some((name, key)) = PATTERNS.iter().find(|(name, _)| rest.starts_with(name)) {
            desc.add(*key)?;
            nodes.push(Node::Key(*key));
            rest = &rest[name.len()..];
        } else if c == '"' {
            // quoted text: each character skips one of the text's; a backslash quotes the next one
            rest = &rest[1..];
            while let Some(mut c) = rest.chars().next() {
                if c == '"' {
                    rest = &rest[1..];
                    break;
                }
                if c == '\\' && rest.len() > 1 {
                    rest = &rest[1..];
                    c = rest.chars().next().unwrap_or(c);
                }
                nodes.push(Node::Skip);
                rest = &rest[c.len_utf8()..];
            }
        } else {
            // outside quotes, a backslash is one only before a double quote
            if c == '\\' && rest[1..].starts_with('"') {
                rest = &rest[1..];
            }
            let c = rest.chars().next().unwrap_or(c);
            nodes.push(Node::Skip);
            rest = &rest[c.len_utf8()..];
        }
    }
    Ok((nodes, desc))
}

/// How many bytes the character that starts with `b` has (`pg_mblen`, for UTF-8).
fn char_len(b: u8) -> usize {
    match b {
        b if b & 0x80 == 0 => 1,
        b if b & 0xe0 == 0xc0 => 2,
        b if b & 0xf0 == 0xe0 => 3,
        b if b & 0xf8 == 0xf0 => 4,
        _ => 1,
    }
}

/// The reading of the text (`NUMProc`).
struct Reader<'a> {
    text: &'a [u8],
    /// where in the text; past its end when something skipped too far
    at: usize,
    /// the sign read: a blank for none
    sign: u8,
    /// the digits read, and the point if there was one
    number: Vec<u8>,
    read_pre: usize,
    read_post: usize,
    read_point: bool,
}

impl Reader<'_> {
    fn byte(&self, i: usize) -> u8 {
        self.text.get(i).copied().unwrap_or(0)
    }

    fn done(&self) -> bool {
        self.at >= self.text.len()
    }

    /// Skips up to `n` characters, but none that could be part of a number.
    fn eat_non_data(&mut self, n: usize) {
        for _ in 0..n {
            if self.done() || b"0123456789.,+-".contains(&self.byte(self.at)) {
                break;
            }
            self.at += char_len(self.byte(self.at));
        }
    }

    /// What `9`, `0`, `.` and `D` do: a blank, a sign before the number, a digit or the point, a
    /// sign after the number (`NUM_numpart_from_char`).
    fn part(&mut self, key: Key, d: &Desc) {
        if self.done() {
            return;
        }
        if self.byte(self.at) == b' ' {
            self.at += 1;
        }
        if self.done() {
            return;
        }
        // the sign before the number: at the first digit pattern
        if self.sign == b' ' && matches!(key, Key::Zero | Key::Nine) && self.read_pre + self.read_post == 0 {
            let c = self.byte(self.at);
            if c == b'-' || (d.bracket && c == b'<') {
                self.sign = b'-';
                self.at += 1;
            } else if c == b'+' {
                self.sign = b'+';
                self.at += 1;
            }
        }
        if self.done() {
            return;
        }
        // a digit or the point
        let mut read = false;
        let c = self.byte(self.at);
        if c.is_ascii_digit() {
            if self.read_point && self.read_post == d.post {
                return;
            }
            self.number.push(c);
            if self.read_point {
                self.read_post += 1;
            } else {
                self.read_pre += 1;
            }
            read = true;
        } else if d.decimal && !self.read_point && c == b'.' {
            self.number.push(b'.');
            self.read_point = true;
            read = true;
        }
        if self.done() {
            return;
        }
        // the sign after the number
        if self.sign == b' ' && self.read_pre + self.read_post > 0 {
            if d.sign && read && self.at + 1 < self.text.len() && !self.byte(self.at + 1).is_ascii_digit() {
                // `S` is anchored to the digit just read
                if matches!(self.byte(self.at + 1), b'-' | b'+') {
                    self.sign = self.byte(self.at + 1);
                    self.at += 1;
                }
            } else if !read && !d.sign && (d.plus || d.minus) {
                // `MI`, `PL` and `SG` when the text doesn't put the sign where the template does
                if matches!(self.byte(self.at), b'-' | b'+') {
                    self.sign = self.byte(self.at);
                }
            }
        }
    }
}

/// What the text says: a sign, digits and maybe a point, and how many of them were before the point
/// and after it.
struct Read {
    sign: u8,
    number: Vec<u8>,
    post: usize,
}

/// Goes along the template and the text together (`NUM_processor`).
fn read(text: &str, nodes: &[Node], d: &Desc) -> Read {
    let mut r = Reader {
        text: text.as_bytes(),
        at: 0,
        sign: b' ',
        number: vec![],
        read_pre: 0,
        read_post: 0,
        read_point: false,
    };
    for node in nodes {
        if r.done() {
            break;
        }
        let Node::Key(key) = *node else {
            r.at += char_len(r.byte(r.at));
            continue;
        };
        match key {
            Key::Nine | Key::Zero | Key::Point | Key::Decimal => r.part(key, d),
            // a comma waits for a comma in the text, and under `FM` never takes one
            Key::Comma | Key::Group => {
                if d.fill || r.byte(r.at) != b',' {
                    continue;
                }
            }
            Key::Local => {
                r.eat_non_data(1);
                continue;
            }
            Key::Minus | Key::Plus | Key::Sign => {
                let c = r.byte(r.at);
                let takes = match key {
                    Key::Minus => c == b'-',
                    Key::Plus => c == b'+',
                    _ => c == b'-' || c == b'+',
                };
                if !takes {
                    r.eat_non_data(1);
                    continue;
                }
                r.sign = c;
            }
            // up to two characters that are no part of a number, but none when the template has a
            // decimal point
            Key::Th => {
                if !d.decimal {
                    r.eat_non_data(2);
                }
                continue;
            }
            _ => continue,
        }
        r.at += 1;
    }
    if r.number.last() == Some(&b'.') {
        r.number.pop();
    }
    Read { sign: r.sign, number: r.number, post: r.read_post }
}

/// A decimal number: its digits, and how many of them are after the point.
struct Dec {
    neg: bool,
    digits: Vec<u8>,
    scale: i64,
}

impl Dec {
    fn is_zero(&self) -> bool {
        self.digits.iter().all(|d| *d == 0)
    }

    /// How many digits the part before the point has once its leading zeros are gone: zero for zero,
    /// and not more than zero for a value between 0 and 1 (minus the zeros after the point).
    fn whole_digits(&self) -> i64 {
        match self.digits.iter().position(|d| *d != 0) {
            Some(leading) => self.digits.len() as i64 - self.scale - leading as i64,
            None => 0,
        }
    }

    /// Rounds (half away from zero) or pads to `scale` digits after the point (`round_var`).
    fn round(&mut self, scale: i64) {
        if scale >= self.scale {
            self.digits.extend(std::iter::repeat_n(0, (scale - self.scale) as usize));
            self.scale = scale;
            return;
        }
        let drop = (self.scale - scale) as usize;
        if drop > self.digits.len() {
            self.digits.clear();
        } else {
            let keep = self.digits.len() - drop;
            let up = self.digits[keep] >= 5;
            self.digits.truncate(keep);
            if up {
                // add one in the last place kept
                let mut i = keep;
                loop {
                    if i == 0 {
                        self.digits.insert(0, 1);
                        break;
                    }
                    i -= 1;
                    if self.digits[i] == 9 {
                        self.digits[i] = 0;
                    } else {
                        self.digits[i] += 1;
                        break;
                    }
                }
            }
        }
        self.scale = scale;
        if self.scale < 0 {
            // a negative scale rounded to tens, hundreds, ...: the value is whole
            if !self.digits.is_empty() {
                self.digits.extend(std::iter::repeat_n(0, (-self.scale) as usize));
            }
            self.scale = 0;
        }
    }

    /// The text of it, with all its scale and no sign for zero.
    fn text(&self) -> String {
        let scale = self.scale as usize;
        let mut digits = self.digits.clone();
        if digits.len() <= scale {
            let pad = scale + 1 - digits.len();
            digits.splice(0..0, std::iter::repeat_n(0, pad));
        }
        let point = digits.len() - scale;
        let whole = digits[..point].iter().position(|d| *d != 0).unwrap_or(point - 1);
        let mut out = String::new();
        if self.neg && !self.is_zero() {
            out.push('-');
        }
        out.extend(digits[whole..point].iter().map(|d| char::from(b'0' + d)));
        if scale > 0 {
            out.push('.');
            out.extend(digits[point..].iter().map(|d| char::from(b'0' + d)));
        }
        out
    }
}

/// The number the reading made, as `numeric_in` makes it with the template's precision and scale,
/// and multiplied by `10^-n` for a `V` (`numeric_to_number`).
fn finish(read: Read, d: &Desc) -> Result<String, Fail> {
    let Read { sign, number, post } = read;
    let mut shown = String::new();
    shown.push(char::from(sign));
    shown.extend(number.iter().map(|b| char::from(*b)));
    if !number.iter().any(u8::is_ascii_digit) {
        return fail(INVALID, format!("invalid input syntax for type numeric: \"{shown}\""));
    }
    let frac = number.iter().position(|b| *b == b'.').map_or(0, |point| number.len() - point - 1);
    let mut dec = Dec {
        neg: sign == b'-',
        digits: number.iter().filter(|b| b.is_ascii_digit()).map(|b| b - b'0').collect(),
        scale: frac as i64,
    };
    // the type: precision digits, `post` of them after the point. Packed into 32 bits as Postgres
    // does, so a template that is too wide wraps as it does.
    let precision = (d.pre + d.multi + post) as u32;
    let typmod = ((precision << 16) | post as u32).wrapping_add(4) as i32;
    if typmod >= 4 {
        let typmod = typmod - 4;
        let precision = i64::from((typmod >> 16) & 0xffff);
        let scale = i64::from(((typmod & 0x7ff) ^ 1024) - 1024);
        dec.round(scale);
        let max_digits = precision - scale;
        if !dec.is_zero() && dec.whole_digits() > max_digits {
            let limit = if max_digits != 0 { format!("10^{max_digits}") } else { "1".into() };
            return fail(
                RANGE,
                format!(
                    "numeric field overflow. A field with precision {precision}, scale {scale} must round to an absolute value less than {limit}."
                ),
            );
        }
    }
    // `make_result`, last in `numeric_in`: what a numeric can hold at all. A typmod that is applied
    // has made sure of it (it rounds to a scale of at most 1023 and bounds the digits), but one that
    // wrapped below 4 is not, and the text's own places and digits are what is left to refuse, zero
    // too (for its places).
    if dec.scale > MAX_DSCALE || dec.whole_digits() > MAX_WHOLE_DIGITS {
        return fail(RANGE, "value overflows numeric format");
    }
    if d.shift {
        // times `10^-multi`, which Postgres computes at 16 more digits than `multi` (up to 1000)
        let rscale = (16 + d.multi).min(1000);
        if d.multi <= rscale {
            dec.digits.extend(std::iter::repeat_n(0, rscale - d.multi));
        } else {
            dec.digits = vec![0];
        }
        dec.scale += rscale as i64;
    }
    Ok(dec.text())
}

/// `to_number(input, fmt)` as text, or `None` for NULL: Postgres answers NULL for an empty template,
/// whatever the input is. (There is no variant that answers plain text: it would have to make
/// something of that NULL.)
pub(super) fn to_number_opt(input: &str, fmt: &str) -> Result<Option<String>, (&'static str, String)> {
    if fmt.is_empty() {
        return Ok(None);
    }
    let (nodes, desc) = parse(fmt)?;
    if desc.exponent {
        return fail(UNSUPPORTED, "\"EEEE\" not supported for input");
    }
    if desc.roman {
        return fail(UNSUPPORTED, "\"RN\" not supported for input");
    }
    finish(read(input, &nodes, &desc), &desc).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Postgres answered.
    enum Want {
        Value(&'static str),
        Error((&'static str, &'static str)),
        Null,
    }
    use Want::*;

    /// `select to_number(input, fmt)` on PostgreSQL 16.13 in the C locale, copied as it came: the
    /// number as text, or the SQLSTATE and message (with the DETAIL, where there is one, after a
    /// full stop), or NULL.
    #[rustfmt::skip]
    const POSTGRES: &[(&str, &str, Want)] = &[
        ("12,345.67", "99G999D99", Value("12345.67")),
        ("12,345.67", "99,999.99", Value("12345.67")),
        ("12345.67", "99999.99", Value("12345.67")),
        ("12.50", "99.99", Value("12.50")),
        ("12", "99.99", Value("12")),
        ("1.20", "9.99", Value("1.20")),
        ("-34,338,492", "99G999G999", Value("-34338492")),
        ("0.1", "0.9", Value("0.1")),
        (".5", ".9", Value("0.5")),
        ("5.", "9.", Value("5")),
        ("123", "999", Value("123")),
        ("0", "9", Value("0")),
        ("00012", "99999", Value("12")),
        ("-0", "9", Value("0")),
        ("-0.00", "9.99", Value("0.00")),
        ("0.00", "9.99", Value("0.00")),
        ("1,234,567.89", "9,999,999.99", Value("1234567.89")),
        ("123456789012345678901234567890.123456789", "999999999999999999999999999999.999999999", Value("123456789012345678901234567890.123456789")),
        ("  12", "999", Value("12")),
        (" 12 ", "999", Value("12")),
        ("12 ", "99", Value("12")),
        ("1 2", "99", Value("12")),
        ("1 2", "9 9", Value("12")),
        ("12345", "99", Value("12")),
        ("1.2345", "9.99", Value("1.23")),
        ("12.345", "99.99", Value("12.34")),
        ("1", "9.99", Value("1")),
        ("1.", "9.99", Value("1")),
        ("123", "9.9", Error(("22003", "numeric field overflow. A field with precision 1, scale 0 must round to an absolute value less than 10^1."))),
        ("12", "9.9", Error(("22003", "numeric field overflow. A field with precision 1, scale 0 must round to an absolute value less than 10^1."))),
        ("1.5", "9", Value("1")),
        ("1.5", "99", Value("1")),
        ("1.2.3", "9.9", Value("1.2")),
        ("1.2.3", "9.9.9", Error(("42601", "multiple decimal points"))),
        ("1..2", "9.99", Value("1.2")),
        ("12", "9.", Error(("22003", "numeric field overflow. A field with precision 1, scale 0 must round to an absolute value less than 10^1."))),
        ("12", ".9", Error(("22003", "numeric field overflow. A field with precision 0, scale 0 must round to an absolute value less than 1."))),
        ("", "999", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("   ", "999", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("", "9", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        (" ", "9", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("abc", "999", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("x", "9", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        (".", "9.9", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("-", "9", Error(("22P02", "invalid input syntax for type numeric: \"-\""))),
        ("+", "9", Error(("22P02", "invalid input syntax for type numeric: \"+\""))),
        ("-.", "9.9", Error(("22P02", "invalid input syntax for type numeric: \"-\""))),
        ("-.5", "9.9", Value("-0.5")),
        ("+.5", "9.9", Value("0.5")),
        ("-", "S9", Error(("22P02", "invalid input syntax for type numeric: \"-\""))),
        ("<", "9PR", Error(("22P02", "invalid input syntax for type numeric: \"-\""))),
        ("", "S", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("12", "S", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("-12", "99", Value("-12")),
        ("+12", "99", Value("12")),
        ("12-", "99MI", Value("-12")),
        ("12+", "99PL", Value("12")),
        ("+12", "PL99", Value("12")),
        ("-12", "S99", Value("-12")),
        ("12-", "99S", Value("-12")),
        ("12-", "99SG", Value("-12")),
        ("-12", "SG99", Value("-12")),
        ("+12", "SG99", Value("12")),
        ("12", "99SG", Value("12")),
        ("-12", "MI99", Value("-12")),
        ("12 -", "99MI", Value("12")),
        ("12 -", "999MI", Value("-12")),
        ("--12", "99", Value("-1")),
        ("- 12", "S99", Value("-1")),
        ("-12", "FMS99", Value("-12")),
        ("12", "S99", Value("12")),
        ("1.2-", "9.9S", Value("-1.2")),
        ("12-", "99", Value("12")),
        ("-12", "99MI", Value("-12")),
        ("<12>", "99PR", Value("-12")),
        ("<12", "99PR", Value("-12")),
        ("12>", "99PR", Value("12")),
        ("12", "99PR", Value("12")),
        ("<1.5>", "9.9PR", Value("-1.5")),
        ("-12", "99PR", Value("-12")),
        ("(12)", "99PR", Value("1")),
        ("-1", "9S", Value("-1")),
        ("1-", "9S", Value("-1")),
        ("1 -", "9S", Value("1")),
        ("+-1", "99", Value("1")),
        ("-+1", "99", Value("-1")),
        ("1+", "9MI", Value("1")),
        ("1-", "9PL", Value("1")),
        ("12345", "999V99", Value("123.450000000000000000")),
        ("123", "999V9", Value("12.30000000000000000")),
        ("12", "9V", Value("1.0000000000000000")),
        ("12", "V9", Value("0.10000000000000000")),
        ("1.5", "9V9", Value("0.10000000000000000")),
        ("0", "9V9", Value("0.00000000000000000")),
        ("1", "9V99999", Value("0.000010000000000000000")),
        ("-12", "S99V9", Value("-1.20000000000000000")),
        ("12", "99V", Value("12.0000000000000000")),
        ("123", "9V9", Value("1.20000000000000000")),
        ("5", "9V999999999999999999999999999999", Value("0.0000000000000000000000000000050000000000000000")),
        ("$12", "\"$\"99", Value("12")),
        ("$12", "$99", Value("12")),
        ("USD 12", "\"USD \"99", Value("12")),
        ("12th", "99th", Value("12")),
        ("12th", "99TH", Value("12")),
        ("1st2", "9th9", Value("12")),
        ("x12", "x99", Value("12")),
        ("1x2", "9x9", Value("12")),
        ("12x", "99x", Value("12")),
        ("x12", "99", Value("1")),
        ("xy12", "99", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("xyz12", "99", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("12", "\"ab\"99", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("ab12", "\"ab\"99", Value("12")),
        ("12", "99\"ab\"", Value("12")),
        ("1\"2", "9\\\"9", Value("12")),
        ("1\"2", "9\"\\\"\"9", Value("12")),
        ("1ab2", "9\"a\\b\"9", Value("12")),
        ("1ab2", "9\"ab", Value("1")),
        ("1.5", "9\".\"9", Value("15")),
        ("1th2", "9TH9", Value("12")),
        ("1th2", "9TH9.", Value("1")),
        ("1th2", "TH99", Value("1")),
        ("xyz12", "TH99", Value("1")),
        ("x12", "TH99", Value("12")),
        ("-1th", "9TH", Value("-1")),
        ("$12", "L99", Value("12")),
        ("12", "L99", Value("12")),
        ("12", "C99", Value("12")),
        ("a12", "L99", Value("12")),
        ("-12", "L99", Value("-12")),
        ("$-12", "L99", Value("-12")),
        ("$ 12", "L99", Value("12")),
        ("€12", "L99", Value("12")),
        ("12.5", "99D9", Value("12.5")),
        ("12,5", "99D9", Error(("22003", "numeric field overflow. A field with precision 2, scale 0 must round to an absolute value less than 10^2."))),
        ("12.5", "99.9", Value("12.5")),
        ("1,234", "9G999", Value("1234")),
        ("1.234", "9G999", Value("123")),
        ("1,234.5", "9G999D9", Value("1234.5")),
        ("12,345", "99,999", Value("12345")),
        ("12,345", "FM99,999", Value("1234")),
        ("12345", "99,999", Value("12345")),
        ("12,345", "99999", Value("1234")),
        ("1,234", "FM9,999", Value("123")),
        ("1,2,3", "9,9,9", Value("123")),
        ("1,2,3", "9G9G9", Value("123")),
        (",12", ",99", Value("12")),
        ("12", "FM99", Value("12")),
        ("12", "fm99", Value("12")),
        ("12", "B99", Value("12")),
        ("12", "99B", Value("12")),
        ("12", "SP99", Value("12")),
        ("12", "9SP9", Value("12")),
        ("12", "0000", Value("12")),
        ("12", "99B99", Value("12")),
        ("12", "FM", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("12", "FM9", Value("1")),
        ("12-", "99mi", Value("-12")),
        ("+12", "pl99", Value("12")),
        ("<12>", "99pr", Value("-12")),
        ("12.5", "99d9", Value("12.5")),
        ("1,234", "9g999", Value("1234")),
        ("$12", "l99", Value("12")),
        ("-12", "s99", Value("-12")),
        ("-12", "sg99", Value("-12")),
        ("12", "9v9", Value("1.20000000000000000")),
        ("1th2", "9th9", Value("12")),
        ("1", "9.9.9", Error(("42601", "multiple decimal points"))),
        ("1", "9D9D9", Error(("42601", "multiple decimal points"))),
        ("1", "9.9D9", Error(("42601", "multiple decimal points"))),
        ("1", "9V9.9", Error(("42601", "cannot use \"V\" and decimal point together"))),
        ("1", "9.9V9", Error(("42601", "cannot use \"V\" and decimal point together"))),
        ("1", "SS9", Error(("42601", "cannot use \"S\" twice"))),
        ("1", "S9S", Error(("42601", "cannot use \"S\" twice"))),
        ("1", "S9MI", Error(("42601", "cannot use \"S\" and \"MI\" together"))),
        ("1", "S9PL", Error(("42601", "cannot use \"S\" and \"PL\" together"))),
        ("1", "S9SG", Error(("42601", "cannot use \"S\" and \"SG\" together"))),
        ("1", "S9PR", Error(("42601", "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together"))),
        ("1", "MI9PR", Error(("42601", "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together"))),
        ("1", "PL9PR", Error(("42601", "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together"))),
        ("1", "SG9PR", Error(("42601", "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together"))),
        ("1", "9PR9", Error(("42601", "\"9\" must be ahead of \"PR\""))),
        ("1", "0PR0", Error(("42601", "\"0\" must be ahead of \"PR\""))),
        ("1", "MI9S", Error(("42601", "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together"))),
        ("1", "PL9S", Error(("42601", "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together"))),
        ("1", "SG9S", Error(("42601", "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together"))),
        ("1", "PR9S", Error(("42601", "\"9\" must be ahead of \"PR\""))),
        ("1", "9EEEE9", Error(("42601", "\"EEEE\" must be the last pattern used"))),
        ("1", "EEEE9", Error(("42601", "\"EEEE\" must be the last pattern used"))),
        ("1", "FM9EEEE", Error(("42601", "\"EEEE\" is incompatible with other formats. \"EEEE\" may only be used together with digit and decimal point patterns."))),
        ("1", "S9EEEE", Error(("42601", "\"EEEE\" is incompatible with other formats. \"EEEE\" may only be used together with digit and decimal point patterns."))),
        ("1", "MI9EEEE", Error(("42601", "\"EEEE\" is incompatible with other formats. \"EEEE\" may only be used together with digit and decimal point patterns."))),
        ("1", "9VEEEE", Error(("42601", "\"EEEE\" is incompatible with other formats. \"EEEE\" may only be used together with digit and decimal point patterns."))),
        ("1", "B9EEEE", Error(("42601", "\"EEEE\" is incompatible with other formats. \"EEEE\" may only be used together with digit and decimal point patterns."))),
        ("1", "RN9EEEE", Error(("42601", "\"EEEE\" is incompatible with other formats. \"EEEE\" may only be used together with digit and decimal point patterns."))),
        ("1", "EEEEEEEE", Error(("42601", "cannot use \"EEEE\" twice"))),
        ("1", "9EEEEB", Error(("42601", "\"EEEE\" must be the last pattern used"))),
        ("1", "EEEE,", Error(("42601", "\"EEEE\" must be the last pattern used"))),
        ("1", "EEEE.", Error(("42601", "\"EEEE\" must be the last pattern used"))),
        ("1", "MIMI9", Value("1")),
        ("1", "PLMI9", Value("1")),
        ("1", "SGSG9", Value("1")),
        ("1", "PRMI", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("12", "EEEE", Error(("0A000", "\"EEEE\" not supported for input"))),
        ("12", "9EEEE", Error(("0A000", "\"EEEE\" not supported for input"))),
        ("12", "9.9EEEE", Error(("0A000", "\"EEEE\" not supported for input"))),
        ("12", "eeee", Error(("0A000", "\"EEEE\" not supported for input"))),
        ("12", "9EEEEx", Error(("0A000", "\"EEEE\" not supported for input"))),
        ("XIV", "RN", Error(("0A000", "\"RN\" not supported for input"))),
        ("14", "RN", Error(("0A000", "\"RN\" not supported for input"))),
        ("14", "rn", Error(("0A000", "\"RN\" not supported for input"))),
        ("14", "9RN", Error(("0A000", "\"RN\" not supported for input"))),
        ("", "RN", Error(("0A000", "\"RN\" not supported for input"))),
        ("", "EEEE", Error(("0A000", "\"EEEE\" not supported for input"))),
        ("x", "RNRN", Error(("0A000", "\"RN\" not supported for input"))),
        ("12", "9.9TH", Error(("22003", "numeric field overflow. A field with precision 1, scale 0 must round to an absolute value less than 10^1."))),
        ("99", "S.99", Error(("22003", "numeric field overflow. A field with precision 0, scale 0 must round to an absolute value less than 1."))),
        ("12.5", ".9", Error(("22003", "numeric field overflow. A field with precision 0, scale 0 must round to an absolute value less than 1."))),
        ("123", "9.99", Error(("22003", "numeric field overflow. A field with precision 1, scale 0 must round to an absolute value less than 10^1."))),
        ("12.3", "9D9", Error(("22003", "numeric field overflow. A field with precision 1, scale 0 must round to an absolute value less than 10^1."))),
        ("12", "", Null),
        ("", "", Null),
        (" ", "", Null),
        ("XIV", "", Null),
        ("日", "", Null),
        ("日12", "\"日\"99", Value("12")),
        ("日12", "日99", Value("12")),
        ("😀12", "\"x\"99", Value("12")),
        ("é12", "x99", Value("12")),
        ("é12", "99", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("1é2", "9x9", Value("12")),
        ("1€2", "9L9", Value("12")),
        ("12é", "99x", Value("12")),
        ("1😀2", "9_9", Value("12")),
        ("😀", "9", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("１２", "99", Error(("22P02", "invalid input syntax for type numeric: \" \""))),
        ("\t12", "99", Value("1")),
        ("12\n", "99", Value("12")),
        ("\n12", "999", Value("12")),
        (" \t12", "999", Value("12")),
    ];

    #[test]
    fn answers_as_postgres_does() {
        let mut wrong = vec![];
        for (input, fmt, want) in POSTGRES {
            let got = to_number_opt(input, fmt);
            let same = match (want, &got) {
                (Value(text), Ok(Some(got))) => text == got,
                (Null, Ok(None)) => true,
                (Error((code, message)), Err((got_code, got_message))) => code == got_code && message == got_message,
                _ => false,
            };
            if !same {
                wrong.push(format!("to_number({input:?}, {fmt:?}) gave {got:?}"));
            }
        }
        assert!(wrong.is_empty(), "{} of {} differ from Postgres:\n{}", wrong.len(), POSTGRES.len(), wrong.join("\n"));
    }

    /// The answer for a template that isn't empty (the one case that is NULL).
    fn number(input: &str, fmt: &str) -> Result<String, Fail> {
        to_number_opt(input, fmt).map(|answer| answer.expect("a template that isn't empty is not NULL"))
    }

    fn text(input: &str, fmt: &str) -> String {
        number(input, fmt).unwrap()
    }

    fn code(input: &str, fmt: &str) -> &'static str {
        number(input, fmt).unwrap_err().0
    }

    #[test]
    fn keeps_the_scale_the_text_had() {
        assert_eq!(text("12.50", "99.99"), "12.50");
        assert_eq!(text("12", "99.99"), "12");
        assert_eq!(text("1.20", "9.99"), "1.20");
        assert_eq!(text("12.345", "99.99"), "12.34");
        assert_eq!(text("-0.00", "9.99"), "0.00");
    }

    #[test]
    fn v_shifts_with_sixteen_more_places() {
        assert_eq!(text("12345", "999V99"), "123.450000000000000000");
        assert_eq!(text("12", "99V"), "12.0000000000000000");
        assert_eq!(text("5", "9V9"), "0.50000000000000000");
        // past 984 places the scale stops at 1000, and 10^-1001 is nothing at that scale
        assert_eq!(text("5", &format!("9V{}", "9".repeat(1000))), format!("0.{}5", "0".repeat(999)));
        assert_eq!(text("5", &format!("9V{}", "9".repeat(1001))), format!("0.{}", "0".repeat(1000)));
    }

    #[test]
    fn an_empty_template_is_null() {
        // whatever the text is, and before it is looked at: there is no answer to give as text
        for input in ["12", "", " ", "XIV", "not a number", "日"] {
            assert_eq!(to_number_opt(input, ""), Ok(None), "to_number({input:?}, '')");
        }
        assert_eq!(to_number_opt("", "9").unwrap_err().0, "22P02");
    }

    #[test]
    fn a_template_is_judged_before_the_text() {
        assert_eq!(code("not a number", "9.9.9"), "42601");
        assert_eq!(code("12", "RN"), "0A000");
        assert_eq!(code("", "EEEE"), "0A000");
    }

    #[test]
    fn wide_templates_wrap_the_type_as_postgres_does() {
        // precision and scale are packed in 32 bits, 16 and 11 of them
        let wide = |n: usize| number(&"1".repeat(n), &"9".repeat(n));
        assert_eq!(wide(32768).unwrap().len(), 32768);
        assert_eq!(wide(65535).unwrap().len(), 65535);
        assert_eq!(
            wide(65536).unwrap_err(),
            (
                "22003",
                "numeric field overflow. A field with precision 0, scale 0 must round to an absolute value less than 1.".to_string()
            )
        );
        assert_eq!(
            wide(70000).unwrap_err(),
            (
                "22003",
                "numeric field overflow. A field with precision 4464, scale 0 must round to an absolute value less than 10^4464."
                    .to_string()
            )
        );
        // 2047 places after the point is a scale of -1: rounded to tens
        let fraction = |n: usize| number(&format!("5.{}", "7".repeat(n)), &format!("9.{}", "9".repeat(n + 3)));
        assert_eq!(fraction(2047).unwrap(), "10");
        assert_eq!(fraction(1023).unwrap().len(), 1025);
        assert_eq!(fraction(1024).unwrap(), "0");
    }

    /// Text or a template made of runs: each piece repeated.
    type Runs = &'static [(&'static str, usize)];

    fn runs(runs: Runs) -> String {
        runs.iter().map(|(piece, times)| piece.repeat(*times)).collect()
    }

    /// Answers of PostgreSQL 16.13 for texts and templates of thousands of characters, which are too
    /// long to write out. `Err` is 22003 "value overflows numeric format" (`make_result`, which has
    /// no DETAIL), `Ok` the number.
    #[rustfmt::skip]
    const LONG: &[(&str, Runs, Runs, Result<Runs, ()>)] = &[
        // a template of 32768 digits or more wraps the packed type below 4 and it isn't applied, so
        // what stops the value is the numeric format: 16383 places and 131072 digits before the point
        ("fraction of 32768 digits", &[(".", 1), ("5", 32768)], &[(".", 1), ("9", 32768)], Err(())),
        ("131073 integer digits", &[("1", 131073)], &[("9", 163840)], Err(())),
        ("131072 integer digits", &[("1", 131072)], &[("9", 163840)], Ok(&[("1", 131072)])),
        ("163840 integer digits", &[("1", 163840)], &[("9", 163840)], Err(())),
        ("16384 places", &[("1", 1), (".", 1), ("5", 16384)], &[("9", 16384), (".", 1), ("9", 16384)], Err(())),
        ("32767 places", &[("1", 1), (".", 1), ("5", 32767)], &[("9", 1), (".", 1), ("9", 32767)], Err(())),
        ("16383 places", &[(".", 1), ("5", 16383)], &[("9", 16385), (".", 1), ("9", 16383)], Ok(&[("0", 1), (".", 1), ("5", 16383)])),
        ("16384 places, another template", &[(".", 1), ("5", 16384)], &[("9", 16385), (".", 1), ("9", 16384)], Err(())),
        // zero has places too
        ("zero with 32767 places", &[("0", 1), (".", 1), ("0", 32767)], &[("9", 1), (".", 1), ("9", 32767)], Err(())),
        ("zero with 16383 places", &[("0", 1), (".", 1), ("0", 16383)], &[("9", 16386), (".", 1), ("9", 16383)], Ok(&[("0", 1), (".", 1), ("0", 16383)])),
        // the digits before the point are counted without the leading zeros, and zero has none
        ("leading zeros", &[("0", 140000), ("1", 1)], &[("9", 163840)], Ok(&[("1", 1)])),
        ("only zeros", &[("0", 131073)], &[("9", 163840)], Ok(&[("0", 1)])),
        // `V` is applied after the type, and `numeric_in` has refused the text by then
        ("131073 integer digits and V", &[("1", 131073)], &[("9", 131073), ("V", 1), ("9", 32767)], Err(())),
        ("131072 integer digits and V", &[("1", 131072)], &[("9", 131072), ("V", 1), ("9", 32768)], Ok(&[("0", 1), (".", 1), ("0", 1000)])),
    ];

    /// An answer for a failure message: a long number is its length and start, not all of it.
    fn brief(answer: &Result<String, Fail>) -> String {
        match answer {
            Ok(value) => {
                format!("{} digits and places, {}...", value.len(), value.chars().take(20).collect::<String>())
            }
            Err((code, message)) => format!("{code} {message}"),
        }
    }

    #[test]
    fn long_texts_are_refused_by_the_numeric_format_as_postgres_does() {
        for (what, input, fmt, want) in LONG {
            let got = number(&runs(input), &runs(fmt));
            let want = match want {
                Ok(value) => Ok(runs(value)),
                Err(()) => Err(("22003", "value overflows numeric format".to_string())),
            };
            assert!(got == want, "{what}: Postgres answers {}, this gave {}", brief(&want), brief(&got));
        }
    }

    #[test]
    fn multibyte_text_is_skipped_whole() {
        assert_eq!(text("日12", "\"日\"99"), "12");
        assert_eq!(text("😀12", "x99"), "12");
        assert_eq!(text("1€2", "9L9"), "12");
        assert_eq!(code("😀", "9"), "22P02");
    }
}
