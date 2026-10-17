//! `.datetime()`: what the method makes, and how it reads, compares and prints.
//!
//! A string is read as a date, time or timestamp by a template, the way `to_timestamp` reads one
//! in its standard mode (`formatting.c`'s `DCH_from_char` and `do_to_timestamp`): only the
//! separators `-./,':; ` and quoted text between the fields, a separator that has to be there, no
//! text left over. Without a template, the ISO forms a date, time, time with a zone, timestamp and
//! timestamp with a zone take are tried one after the other. Times print as jsonb prints them (an
//! ISO 8601 `T` between date and time, a zone as `+01:00`), and a timestamp with a zone in the
//! session's zone.

use super::PathError;
use std::cmp::Ordering;
use std::fmt;

/// A date, time or timestamp, as `.datetime()` makes one, in Postgres's own terms: dates are days
/// and timestamps microseconds since 2000-01-01.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Datetime {
    Date(i32),
    /// microseconds since midnight
    Time(i64),
    /// microseconds since midnight, and the zone in seconds *west* of UTC (as Postgres keeps it)
    TimeTz(i64, i32),
    Timestamp(i64),
    /// microseconds since 2000-01-01 00:00 UTC, and the zone it was written in (seconds west), which is
    /// the one it prints in
    TimestampTz(i64, i32),
}

const USECS_PER_SEC: i64 = 1_000_000;
const USECS_PER_DAY: i64 = 86_400 * USECS_PER_SEC;
const SECS_PER_DAY: i64 = 86_400;
/// 2000-01-01 as a Julian day
const POSTGRES_EPOCH_JDATE: i64 = 2_451_545;
const MAX_TZDISP_HOUR: i32 = 15;

// ---------------------------------------------------------------------------------------------
// the calendar

/// A date (year 0 is 1 BC) as a Julian day number.
fn date2j(year: i64, month: i64, day: i64) -> i64 {
    let (mut y, mut m) = (year, month);
    if m > 2 {
        m += 1;
        y += 4800;
    } else {
        m += 13;
        y += 4799;
    }
    let century = y / 100;
    let mut julian = y * 365 - 32167;
    julian += y / 4 - century + century / 4;
    julian += 7834 * m / 256 + day;
    julian
}

/// A Julian day number as (year, month, day).
fn j2date(jd: i64) -> (i64, i64, i64) {
    let mut julian = (jd + 32044) as u64;
    let quad = julian / 146_097;
    let extra = (julian - quad * 146_097) * 4 + 3;
    julian += 60 + quad * 3 + extra / 146_097;
    let quad = julian / 1461;
    julian -= quad * 1461;
    let mut y = (julian * 4 / 1461) as i64;
    julian = (if y != 0 { (julian + 305) % 365 } else { (julian + 306) % 366 }) + 123;
    y += (quad * 4) as i64;
    let year = y - 4800;
    let quad = julian * 2141 / 65536;
    let day = julian - 7834 * quad / 256;
    let month = (quad + 10) % 12 + 1;
    (year, month as i64, day as i64)
}

/// The day of the week of a Julian day number: 0 is Sunday.
fn j2day(date: i64) -> i64 {
    (date + 1).rem_euclid(7)
}

fn isleap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// The Julian day number of the first day of ISO week `week` of `year`.
fn isoweek2j(year: i64, week: i64) -> i64 {
    let day4 = date2j(year, 1, 4);
    let day0 = j2day(day4 - 1);
    (week - 1) * 7 + (day4 - day0)
}

/// The earliest and latest Julian days a date may be (4714-11-24 BC, 5874897-12-31), and the first
/// timestamp too early (4714-11-24 BC) and the first too late (294277-01-01), in microseconds since 2000.
const JULIAN_MIN: i64 = 0;
const JULIAN_MAX: i64 = 2_147_483_494;
const TS_MIN: i64 = -211_813_488_000_000_000;
const TS_END: i64 = 9_223_371_331_200_000_000;

/// `IS_VALID_JULIAN`: from 4714-11-24 BC to 5874897-06-03 AD... as Postgres counts them.
fn valid_julian(y: i64, m: i64, d: i64) -> bool {
    (y > -4713 || (y == -4713 && (m > 11 || (m == 11 && d >= 24))))
        && (y < 5_874_898 || (y == 5_874_898 && (m < 6 || (m == 6 && d <= 3))))
}

// ---------------------------------------------------------------------------------------------
// what a template is made of

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    None,
    Gregorian,
    IsoWeek,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Ad,
    Am,
    Bc,
    Cc,
    Day,
    Ddd,
    Dd,
    Dy,
    D,
    Ff(u8),
    Fx,
    Hh12,
    Hh24,
    Hh,
    Iddd,
    Id,
    Iw,
    Iyyy,
    Iyy,
    Iy,
    I,
    J,
    Mi,
    Mm,
    Month,
    Mon,
    Ms,
    Of,
    Pm,
    Q,
    Rm,
    Ssss,
    Ss,
    Tzh,
    Tzm,
    Tz,
    Us,
    Ww,
    W,
    YYyy,
    Yyyy,
    Yyy,
    Yy,
    Y,
}

/// A template word: its text, what it is, whether it reads digits, and the calendar it belongs to.
#[derive(Debug)]
struct Word {
    name: &'static str,
    key: Key,
    digit: bool,
    mode: Mode,
    /// the long forms with dots, `A.M.` and `B.C.`
    dots: bool,
    /// the case it asks for: 0 upper, 1 capitalised, 2 lower
    case: u8,
}

const fn w(name: &'static str, key: Key, digit: bool, mode: Mode, dots: bool, case: u8) -> Word {
    Word { name, key, digit, mode, dots, case }
}

use Mode::{Gregorian as G, IsoWeek as I8, None as N};

/// The words, in the order Postgres tries them for a letter: a longer word before a shorter one it
/// starts with.
static WORDS: &[Word] = &[
    w("A.D.", Key::Ad, false, N, true, 0),
    w("A.M.", Key::Am, false, N, true, 0),
    w("AD", Key::Ad, false, N, false, 0),
    w("AM", Key::Am, false, N, false, 0),
    w("B.C.", Key::Bc, false, N, true, 0),
    w("BC", Key::Bc, false, N, false, 0),
    w("CC", Key::Cc, true, N, false, 0),
    w("DAY", Key::Day, false, N, false, 0),
    w("DDD", Key::Ddd, true, G, false, 0),
    w("DD", Key::Dd, true, G, false, 0),
    w("DY", Key::Dy, false, N, false, 0),
    w("Day", Key::Day, false, N, false, 1),
    w("Dy", Key::Dy, false, N, false, 1),
    w("D", Key::D, true, G, false, 0),
    w("FF1", Key::Ff(1), true, N, false, 0),
    w("FF2", Key::Ff(2), true, N, false, 0),
    w("FF3", Key::Ff(3), true, N, false, 0),
    w("FF4", Key::Ff(4), true, N, false, 0),
    w("FF5", Key::Ff(5), true, N, false, 0),
    w("FF6", Key::Ff(6), true, N, false, 0),
    w("FX", Key::Fx, false, N, false, 0),
    w("HH12", Key::Hh12, true, N, false, 0),
    w("HH24", Key::Hh24, true, N, false, 0),
    w("HH", Key::Hh, true, N, false, 0),
    w("IDDD", Key::Iddd, true, I8, false, 0),
    w("ID", Key::Id, true, I8, false, 0),
    w("IW", Key::Iw, true, I8, false, 0),
    w("IYYY", Key::Iyyy, true, I8, false, 0),
    w("IYY", Key::Iyy, true, I8, false, 0),
    w("IY", Key::Iy, true, I8, false, 0),
    w("I", Key::I, true, I8, false, 0),
    w("J", Key::J, true, N, false, 0),
    w("MI", Key::Mi, true, N, false, 0),
    w("MM", Key::Mm, true, G, false, 0),
    w("MONTH", Key::Month, false, G, false, 0),
    w("MON", Key::Mon, false, G, false, 0),
    w("MS", Key::Ms, true, N, false, 0),
    w("Month", Key::Month, false, G, false, 1),
    w("Mon", Key::Mon, false, G, false, 1),
    w("OF", Key::Of, false, N, false, 0),
    w("P.M.", Key::Pm, false, N, true, 0),
    w("PM", Key::Pm, false, N, false, 0),
    w("Q", Key::Q, true, N, false, 0),
    w("RM", Key::Rm, false, G, false, 0),
    w("SSSSS", Key::Ssss, true, N, false, 0),
    w("SSSS", Key::Ssss, true, N, false, 0),
    w("SS", Key::Ss, true, N, false, 0),
    w("TZH", Key::Tzh, false, N, false, 0),
    w("TZM", Key::Tzm, true, N, false, 0),
    w("TZ", Key::Tz, false, N, false, 0),
    w("US", Key::Us, true, N, false, 0),
    w("WW", Key::Ww, true, G, false, 0),
    w("W", Key::W, true, G, false, 0),
    w("Y,YYY", Key::YYyy, true, G, false, 0),
    w("YYYY", Key::Yyyy, true, G, false, 0),
    w("YYY", Key::Yyy, true, G, false, 0),
    w("YY", Key::Yy, true, G, false, 0),
    w("Y", Key::Y, true, G, false, 0),
    // the lower-case words
    w("a.d.", Key::Ad, false, N, true, 2),
    w("a.m.", Key::Am, false, N, true, 2),
    w("ad", Key::Ad, false, N, false, 2),
    w("am", Key::Am, false, N, false, 2),
    w("b.c.", Key::Bc, false, N, true, 2),
    w("bc", Key::Bc, false, N, false, 2),
    w("cc", Key::Cc, true, N, false, 2),
    w("day", Key::Day, false, N, false, 2),
    w("ddd", Key::Ddd, true, G, false, 2),
    w("dd", Key::Dd, true, G, false, 2),
    w("dy", Key::Dy, false, N, false, 2),
    w("d", Key::D, true, G, false, 2),
    w("ff1", Key::Ff(1), true, N, false, 2),
    w("ff2", Key::Ff(2), true, N, false, 2),
    w("ff3", Key::Ff(3), true, N, false, 2),
    w("ff4", Key::Ff(4), true, N, false, 2),
    w("ff5", Key::Ff(5), true, N, false, 2),
    w("ff6", Key::Ff(6), true, N, false, 2),
    w("fx", Key::Fx, false, N, false, 2),
    w("hh12", Key::Hh12, true, N, false, 2),
    w("hh24", Key::Hh24, true, N, false, 2),
    w("hh", Key::Hh, true, N, false, 2),
    w("iddd", Key::Iddd, true, I8, false, 2),
    w("id", Key::Id, true, I8, false, 2),
    w("iw", Key::Iw, true, I8, false, 2),
    w("iyyy", Key::Iyyy, true, I8, false, 2),
    w("iyy", Key::Iyy, true, I8, false, 2),
    w("iy", Key::Iy, true, I8, false, 2),
    w("i", Key::I, true, I8, false, 2),
    w("j", Key::J, true, N, false, 2),
    w("mi", Key::Mi, true, N, false, 2),
    w("mm", Key::Mm, true, G, false, 2),
    w("month", Key::Month, false, G, false, 2),
    w("mon", Key::Mon, false, G, false, 2),
    w("ms", Key::Ms, true, N, false, 2),
    w("of", Key::Of, false, N, false, 2),
    w("p.m.", Key::Pm, false, N, true, 2),
    w("pm", Key::Pm, false, N, false, 2),
    w("q", Key::Q, true, N, false, 2),
    w("rm", Key::Rm, false, G, false, 2),
    w("sssss", Key::Ssss, true, N, false, 2),
    w("ssss", Key::Ssss, true, N, false, 2),
    w("ss", Key::Ss, true, N, false, 2),
    w("tzh", Key::Tzh, false, N, false, 2),
    w("tzm", Key::Tzm, true, N, false, 2),
    w("tz", Key::Tz, false, N, false, 2),
    w("us", Key::Us, true, N, false, 2),
    w("ww", Key::Ww, true, G, false, 2),
    w("w", Key::W, true, G, false, 2),
    w("y,yyy", Key::YYyy, true, G, false, 2),
    w("yyyy", Key::Yyyy, true, G, false, 2),
    w("yyy", Key::Yyy, true, G, false, 2),
    w("yy", Key::Yy, true, G, false, 2),
    w("y", Key::Y, true, G, false, 2),
];

#[derive(Clone, Copy, Debug)]
enum Kind {
    Action(&'static Word),
    Space,
    Separator,
    Char,
}

/// A part of a template.
#[derive(Clone, Debug)]
struct Fmt {
    kind: Kind,
    /// the character of a space, a separator or plain text
    ch: String,
    /// the `FM` prefix, and the `TH`/`th`/`SP` suffixes
    fm: bool,
    th: bool,
}

type DR<X> = Result<X, PathError>;

/// Why a string is no date or time by a template: an error `silent` passes over, or one it does
/// not (a template that is no template).
#[derive(Debug)]
pub enum DtErr {
    Soft(PathError),
    Hard(PathError),
}

impl DtErr {
    pub fn parts(self) -> (bool, PathError) {
        match self {
            DtErr::Soft(e) => (false, e),
            DtErr::Hard(e) => (true, e),
        }
    }
}

impl From<PathError> for DtErr {
    fn from(e: PathError) -> DtErr {
        DtErr::Soft(e)
    }
}

fn bad(code: &'static str, msg: impl Into<String>) -> PathError {
    (code, msg.into())
}

fn fmt_err(msg: impl Into<String>) -> PathError {
    bad("22007", msg)
}

/// `is_separator_char`: a printable ASCII character that is no letter or digit.
fn is_sep(c: u8) -> bool {
    c > 32 && c < 127 && !c.is_ascii_alphanumeric()
}

fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn find_word(rest: &str) -> Option<&'static Word> {
    let c = *rest.as_bytes().first()?;
    if !(b' '..127).contains(&c) {
        return None;
    }
    WORDS.iter().find(|w| rest.starts_with(w.name))
}

/// Reads a template: strictly, as jsonpath's `.datetime(template)` does.
fn parse_format(template: &str) -> Result<Vec<Fmt>, DtErr> {
    let mut out = vec![];
    let mut s = template;
    while !s.is_empty() {
        let mut fm = false;
        if s.starts_with("FM") || s.starts_with("fm") {
            fm = true;
            s = &s[2..];
        } else if s.starts_with("TM") || s.starts_with("tm") {
            s = &s[2..];
        }
        if !s.is_empty()
            && let Some(word) = find_word(s)
        {
            s = &s[word.name.len()..];
            let mut th = false;
            if s.starts_with("TH") || s.starts_with("th") {
                th = true;
                s = &s[2..];
            } else if s.starts_with("SP") {
                s = &s[2..];
            }
            out.push(Fmt { kind: Kind::Action(word), ch: String::new(), fm, th });
        } else if let Some(c) = s.chars().next() {
            // (a prefix with no word after it is only a prefix, dropped, as it is in Postgres)
            if c != '"' {
                if !"-./,':; ".contains(c) {
                    return Err(DtErr::Hard(fmt_err(format!("invalid datetime format separator: \"{c}\""))));
                }
                out.push(Fmt {
                    kind: if c == ' ' { Kind::Space } else { Kind::Separator },
                    ch: c.to_string(),
                    fm: false,
                    th: false,
                });
                s = &s[1..];
            } else {
                s = &s[1..];
                while let Some(c) = s.chars().next() {
                    if c == '"' {
                        s = &s[1..];
                        break;
                    }
                    if c == '\\' && s.len() > 1 {
                        s = &s[1..];
                    }
                    let c = s.chars().next().unwrap_or(c);
                    out.push(Fmt { kind: Kind::Char, ch: c.to_string(), fm: false, th: false });
                    s = &s[c.len_utf8()..];
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// reading a string by a template

/// What the fields of a string were (TmFromChar).
#[derive(Default)]
struct Fields {
    mode: Option<Mode>,
    hh: i64,
    pm: i64,
    mi: i64,
    ss: i64,
    ssss: i64,
    d: i64,
    dd: i64,
    ddd: i64,
    mm: i64,
    ms: i64,
    year: i64,
    bc: i64,
    ww: i64,
    w: i64,
    cc: i64,
    q: i64,
    j: i64,
    us: i64,
    yysz: i64,
    clock_12_hour: i64,
    tzsign: i64,
    tzh: i64,
    tzm: i64,
    ff: i64,
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const ROMAN: [&str; 12] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII"];

/// A reader over the input, with the byte position `s`.
struct Input<'a> {
    text: &'a str,
    s: usize,
}

impl Input<'_> {
    fn rest(&self) -> &str {
        &self.text[self.s..]
    }

    fn byte(&self, k: usize) -> u8 {
        self.text.as_bytes().get(self.s + k).copied().unwrap_or(0)
    }

    fn eof(&self) -> bool {
        self.s >= self.text.len()
    }

    /// Steps over one character.
    fn skip_char(&mut self) {
        if let Some(c) = self.rest().chars().next() {
            self.s += c.len_utf8();
        }
    }
}

/// `strtol(init, &end, 10)` of `text`: the value (saturated) and how many bytes it used; none used
/// if there is no number.
fn strtol(text: &str) -> (i64, usize, bool) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() && is_c_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let digits_from = i;
    let mut v: i128 = 0;
    let mut overflow = false;
    while i < b.len() && b[i].is_ascii_digit() {
        v = v * 10 + i128::from(b[i] - b'0');
        if v > i128::from(i64::MAX) + 1 {
            v = i128::from(i64::MAX) + 1;
            overflow = true;
        }
        i += 1;
    }
    if i == digits_from {
        return (0, 0, false);
    }
    let v = if neg { -v } else { v };
    let clamped = v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    (clamped, i, overflow || v > i128::from(i64::MAX) || v < i128::from(i64::MIN))
}

/// `sscanf("%<width>d")`: blanks, an optional sign, digits, all in `width` characters.
fn scan_int(text: &str, width: usize) -> Option<i64> {
    let t = text.trim_start_matches(|c: char| c.is_ascii() && is_c_space(c as u8));
    let b = t.as_bytes();
    let (mut i, mut neg) = (0, false);
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let from = i;
    let mut v: i64 = 0;
    while i < b.len() && i < width && b[i].is_ascii_digit() {
        v = v * 10 + i64::from(b[i] - b'0');
        i += 1;
    }
    (i > from).then_some(if neg { -v } else { v })
}

fn set_int(dest: &mut i64, value: i64, name: &str) -> DR<()> {
    if *dest != 0 && *dest != value {
        return Err(fmt_err(format!("conflicting values for \"{name}\" field in formatting string")));
    }
    *dest = value;
    Ok(())
}

/// Whether the node after `n` (or `n` itself, with a `TH`) is something other than a number: then
/// a number field takes all the digits there are.
fn is_next_separator(fmts: &[Fmt], n: usize) -> bool {
    if fmts[n].th && matches!(fmts[n].kind, Kind::Action(_)) {
        return true;
    }
    match fmts.get(n + 1) {
        None => true,
        Some(f) => match f.kind {
            Kind::Action(word) => !word.digit,
            _ => !(f.ch.len() == 1 && f.ch.as_bytes()[0].is_ascii_digit()),
        },
    }
}

/// `seq_search_ascii`: the index of the name the text starts with, ignoring case, and how long it is.
fn seq_search(text: &str, names: &[&str]) -> Option<(usize, usize)> {
    names
        .iter()
        .position(|n| text.len() >= n.len() && text.as_bytes()[..n.len()].eq_ignore_ascii_case(n.as_bytes()))
        .map(|i| (i, names[i].len()))
}

fn invalid_value(init: &str, name: &str, n: usize) -> PathError {
    let cut: String = init.chars().take(n).collect();
    not_integer(&cut, name)
}

/// What `from_char_parse_int_len` says of a field with no digits (its DETAIL after a `\u{1}`).
fn not_integer(copy: &str, name: &str) -> PathError {
    fmt_err(format!("invalid value \"{copy}\" for \"{name}\"\u{1}Value must be an integer."))
}

/// What `from_char_seq_search` says of a field that is none of its names (its DETAIL after a `\u{1}`).
fn no_match(copy: &str, name: &str) -> PathError {
    fmt_err(format!(
        "invalid value \"{copy}\" for \"{name}\"\u{1}The given value did not match any of the allowed values for this \
         field."
    ))
}

/// The HINT of a fixed-width field that is short (after a `\u{2}`).
const FM_HINT: &str = "\u{2}If your source string is not fixed-width, try using the \"FM\" modifier.";

impl Fields {
    fn set_mode(&mut self, mode: Mode) -> DR<()> {
        if mode != Mode::None {
            match self.mode {
                None => self.mode = Some(mode),
                Some(m) if m != mode => {
                    return Err(fmt_err("invalid combination of date conventions"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Reads `len` digits (or all there are, if the field is followed by something that is no digit),
/// as from_char_parse_int_len does: the value, and how many bytes it took. Blanks before the number
/// are skipped; in a field of fixed width, blanks and nothing after them are a zero.
fn parse_int(inp: &mut Input, len: usize, fmts: &[Fmt], n: usize, name: &str) -> DR<(i64, usize)> {
    let Kind::Action(_) = fmts[n].kind else { unreachable!("a number is read by a word") };
    let init = inp.s;
    let mut skipped = 0;
    while !inp.eof() && is_c_space(inp.byte(0)) {
        inp.s += 1;
        skipped += 1;
    }
    let post = inp.s;
    let cut = |rest: &str, n: usize| -> String {
        let mut end = n.min(rest.len());
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        rest[..end].to_string()
    };
    let (result, overflow);
    if fmts[n].fm || is_next_separator(fmts, n) {
        // all the digits there are
        let (v, used, over) = strtol(inp.rest());
        if used == 0 {
            let shown = cut(inp.rest(), len);
            return Err(invalid_value(&shown, name, shown.chars().count()));
        }
        result = v;
        overflow = over;
        inp.s += used;
    } else {
        // exactly `len` characters
        let rest = inp.rest();
        if rest.len() < len {
            return Err(fmt_err(format!(
                "source string too short for \"{name}\" formatting field\u{1}Field requires {len} characters, but \
                 only {} remain.{FM_HINT}",
                rest.len()
            )));
        }
        let copy = cut(rest, len);
        let (v, used, over) = strtol(&copy);
        if used == 0 {
            if skipped == 0 {
                return Err(not_integer(&copy, name));
            }
            // the blanks were the field: no digits, a zero
            return Ok((0, post - init));
        }
        if used < len {
            return Err(fmt_err(format!(
                "invalid value \"{copy}\" for \"{name}\"\u{1}Field requires {len} characters, but only {used} \
                 could be parsed.{FM_HINT}"
            )));
        }
        result = v;
        overflow = over;
        inp.s += used;
    }
    if overflow || result < i64::from(i32::MIN) || result > i64::from(i32::MAX) {
        return Err(bad(
            "22008",
            format!(
                "value for \"{name}\" in source string is out of range\u{1}Value must be in the range -2147483648 \
                 to 2147483647."
            ),
        ));
    }
    Ok((result, inp.s - init))
}

/// Skips the two characters after a number with `TH`.
fn skip_th(inp: &mut Input, f: &Fmt) {
    if f.th {
        inp.skip_char();
        inp.skip_char();
    }
}

fn adjust_partial_year(year: i64) -> i64 {
    if year < 70 {
        year + 2000
    } else if year < 100 {
        year + 1900
    } else if year < 520 {
        year + 2000
    } else if year < 1000 {
        year + 1000
    } else {
        year
    }
}

/// Reads `text` by `fmts` into fields (DCH_from_char, strictly).
fn from_char(fmts: &[Fmt], text: &str) -> DR<Fields> {
    let mut out = Fields::default();
    let mut inp = Input { text, s: 0 };
    let mut fx_mode = true;
    let std = true;
    let mut extra_skip: i64 = 0;
    let mut n = 0;
    while n < fmts.len() && !inp.eof() {
        let f = &fmts[n];
        if !fx_mode
            && (!matches!(f.kind, Kind::Action(w) if w.key == Key::Fx))
            && (matches!(f.kind, Kind::Action(_)) || n == 0)
        {
            while !inp.eof() && is_c_space(inp.byte(0)) {
                inp.s += 1;
                extra_skip += 1;
            }
        }
        match f.kind {
            Kind::Space | Kind::Separator => {
                if std {
                    let want = f.ch.as_bytes()[0];
                    if inp.byte(0) == want && !inp.eof() {
                        inp.s += 1;
                    } else {
                        return Err(fmt_err(format!("unmatched format separator \"{}\"", f.ch)));
                    }
                }
                n += 1;
                continue;
            }
            Kind::Char => {
                // strictly, the text has to be there
                if inp.rest().starts_with(f.ch.as_str()) {
                    inp.s += f.ch.len();
                } else {
                    return Err(fmt_err(format!("unmatched format character \"{}\"", f.ch)));
                }
                n += 1;
                continue;
            }
            Kind::Action(_) => {}
        }
        let Kind::Action(word) = f.kind else { unreachable!() };
        out.set_mode(word.mode)?;
        let name = word.name;
        match word.key {
            Key::Fx => fx_mode = true,
            Key::Am | Key::Pm => {
                let names: &[&str] =
                    if word.dots { &["a.m.", "p.m.", "A.M.", "P.M."] } else { &["am", "pm", "AM", "PM"] };
                let Some((i, l)) = seq_search(inp.rest(), names) else {
                    return Err(no_match(&seq_copy(inp.rest(), names), name));
                };
                inp.s += l;
                set_int(&mut out.pm, (i % 2) as i64, name)?;
                out.clock_12_hour = 1;
            }
            Key::Ad | Key::Bc => {
                let names: &[&str] =
                    if word.dots { &["a.d.", "b.c.", "A.D.", "B.C."] } else { &["ad", "bc", "AD", "BC"] };
                let Some((i, l)) = seq_search(inp.rest(), names) else {
                    return Err(no_match(&seq_copy(inp.rest(), names), name));
                };
                inp.s += l;
                set_int(&mut out.bc, (i % 2) as i64, name)?;
            }
            Key::Hh | Key::Hh12 => {
                out.clock_12_hour = 1;
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.hh, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::Hh24 => {
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.hh, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::Mi => {
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.mi, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::Ss => {
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.ss, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::Ms => {
                let (v, len) = parse_int(&mut inp, 3, fmts, n, name)?;
                set_int(&mut out.ms, v, name)?;
                out.ms *= match len {
                    1 => 100,
                    2 => 10,
                    _ => 1,
                };
                skip_th(&mut inp, f);
            }
            Key::Ff(_) | Key::Us => {
                let len = if let Key::Ff(k) = word.key {
                    out.ff = i64::from(k);
                    usize::from(k)
                } else {
                    6
                };
                let (v, l) = parse_int(&mut inp, len, fmts, n, name)?;
                set_int(&mut out.us, v, name)?;
                out.us *= match l {
                    1 => 100_000,
                    2 => 10_000,
                    3 => 1000,
                    4 => 100,
                    5 => 10,
                    _ => 1,
                };
                skip_th(&mut inp, f);
            }
            Key::Ssss => {
                let (v, _) = parse_int(&mut inp, name.len(), fmts, n, name)?;
                set_int(&mut out.ssss, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::Tz | Key::Of => {
                return Err(bad("0A000", format!("formatting field \"{name}\" is only supported in to_char")));
            }
            Key::Tzh => {
                let c = inp.byte(0);
                if !inp.eof() && (c == b'+' || c == b'-' || c == b' ') {
                    out.tzsign = if c == b'-' { -1 } else { 1 };
                    inp.s += 1;
                } else if extra_skip > 0 && inp.s > 0 && inp.text.as_bytes()[inp.s - 1] == b'-' {
                    out.tzsign = -1;
                } else {
                    out.tzsign = 1;
                }
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.tzh, v, name)?;
            }
            Key::Tzm => {
                if out.tzsign == 0 {
                    out.tzsign = 1;
                }
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.tzm, v, name)?;
            }
            Key::Month | Key::Mon => {
                let full = word.key == Key::Month;
                let names: Vec<&str> = if full { MONTHS.to_vec() } else { MONTHS.iter().map(|m| &m[..3]).collect() };
                let Some((i, l)) = seq_search(inp.rest(), &names) else {
                    return Err(no_match(&seq_copy(inp.rest(), &names), name));
                };
                inp.s += l;
                set_int(&mut out.mm, i as i64 + 1, name)?;
            }
            Key::Rm => {
                // upper-case names, the longest first, as Postgres lists them
                let names = ["XII", "XI", "X", "IX", "VIII", "VII", "VI", "V", "IV", "III", "II", "I"];
                let Some((i, l)) = seq_search(inp.rest(), &names) else {
                    return Err(no_match(&seq_copy(inp.rest(), &names), name));
                };
                inp.s += l;
                set_int(&mut out.mm, 12 - i as i64, name)?;
            }
            Key::Day | Key::Dy => {
                let full = word.key == Key::Day;
                let names: Vec<&str> = if full { DAYS.to_vec() } else { DAYS.iter().map(|d| &d[..3]).collect() };
                let Some((i, l)) = seq_search(inp.rest(), &names) else {
                    return Err(no_match(&seq_copy(inp.rest(), &names), name));
                };
                inp.s += l;
                set_int(&mut out.d, i as i64, name)?;
                out.d += 1;
            }
            Key::Ddd | Key::Iddd => {
                let (v, _) = parse_int(&mut inp, 3, fmts, n, name)?;
                set_int(&mut out.ddd, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::Dd => {
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.dd, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::Mm => {
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.mm, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::D => {
                let (v, _) = parse_int(&mut inp, 1, fmts, n, name)?;
                set_int(&mut out.d, v, name)?;
                out.d -= 1;
                skip_th(&mut inp, f);
            }
            Key::Id => {
                let (v, _) = parse_int(&mut inp, 1, fmts, n, name)?;
                set_int(&mut out.d, v, name)?;
                // shift the numbering to Gregorian, where Sunday is 1
                out.d += 1;
                if out.d > 7 {
                    out.d = 1;
                }
                skip_th(&mut inp, f);
            }
            Key::Ww | Key::Iw => {
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.ww, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::Q => {
                // the quarter is read and ignored: it would only contradict a month
                parse_int(&mut inp, 1, fmts, n, name)?;
                skip_th(&mut inp, f);
            }
            Key::Cc => {
                let (v, _) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.cc, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::W => {
                let (v, _) = parse_int(&mut inp, 1, fmts, n, name)?;
                set_int(&mut out.w, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::J => {
                let (v, _) = parse_int(&mut inp, name.len(), fmts, n, name)?;
                set_int(&mut out.j, v, name)?;
                skip_th(&mut inp, f);
            }
            Key::YYyy => {
                // sscanf(s, "%d,%03d", &millennia, &years)
                let rest = inp.rest();
                let (millennia, used, _) = strtol(rest);
                let after = &rest[used.min(rest.len())..];
                let matched = used > 0 && after.starts_with(',');
                let years = if matched { scan_int(&after[1..], 3) } else { None };
                let Some(years) = years else {
                    return Err(fmt_err("invalid input string for \"Y,YYY\""));
                };
                set_int(&mut out.year, years + millennia * 1000, name)?;
                out.yysz = 4;
                // on to the comma, and four characters more
                inp.s += rest.find(',').unwrap_or(rest.len()) + 4;
                inp.s = inp.s.min(inp.text.len());
                while !inp.text.is_char_boundary(inp.s) {
                    inp.s += 1;
                }
                skip_th(&mut inp, f);
            }
            Key::Yyyy | Key::Iyyy => {
                let (v, _) = parse_int(&mut inp, 4, fmts, n, name)?;
                set_int(&mut out.year, v, name)?;
                out.yysz = 4;
                skip_th(&mut inp, f);
            }
            Key::Yyy | Key::Iyy => {
                let (v, len) = parse_int(&mut inp, 3, fmts, n, name)?;
                set_int(&mut out.year, v, name)?;
                if len < 4 {
                    out.year = adjust_partial_year(out.year);
                }
                out.yysz = 3;
                skip_th(&mut inp, f);
            }
            Key::Yy | Key::Iy => {
                let (v, len) = parse_int(&mut inp, 2, fmts, n, name)?;
                set_int(&mut out.year, v, name)?;
                if len < 4 {
                    out.year = adjust_partial_year(out.year);
                }
                out.yysz = 2;
                skip_th(&mut inp, f);
            }
            Key::Y | Key::I => {
                let (v, len) = parse_int(&mut inp, 1, fmts, n, name)?;
                set_int(&mut out.year, v, name)?;
                if len < 4 {
                    out.year = adjust_partial_year(out.year);
                }
                out.yysz = 1;
                skip_th(&mut inp, f);
            }
        }
        n += 1;
    }
    // strict parsing doesn't allow unmatched format patterns, or text left over
    if n < fmts.len() {
        return Err(fmt_err("input string is too short for datetime format"));
    }
    while !inp.eof() && is_c_space(inp.byte(0)) {
        inp.s += 1;
    }
    if !inp.eof() {
        return Err(fmt_err("trailing characters remain in input string after datetime format"));
    }
    Ok(out)
}

/// The text of a value that did not match, as Postgres quotes it: up to the first blank.
fn seq_copy(text: &str, _names: &[&str]) -> String {
    text.chars().take_while(|&c| !(c.is_ascii() && is_c_space(c as u8))).collect()
}

/// What a template has fields of: dates, times, zones.
const DATED: u32 = 1;
const TIMED: u32 = 2;
const ZONED: u32 = 4;

fn datetime_type(fmts: &[Fmt]) -> u32 {
    let mut flags = 0;
    for f in fmts {
        let Kind::Action(w) = f.kind else { continue };
        flags |= match w.key {
            // (Postgres leaves IW out of the keys that make a date: `IW` alone is neither dated nor timed)
            Key::Fx | Key::Iw => 0,
            Key::Am
            | Key::Pm
            | Key::Hh
            | Key::Hh12
            | Key::Hh24
            | Key::Mi
            | Key::Ss
            | Key::Ms
            | Key::Us
            | Key::Ff(_)
            | Key::Ssss => TIMED,
            Key::Tz | Key::Of | Key::Tzh | Key::Tzm => ZONED,
            _ => DATED,
        };
    }
    flags
}

/// The text a date/time error quotes, for a field out of range.
fn field_overflow(text: &str) -> PathError {
    bad("22008", format!("date/time field value out of range: \"{text}\""))
}

/// A parsed template's result: the fields as a date and time of day (do_to_timestamp).
struct Tm {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
    fsec: i64,
    /// the zone, in seconds west of UTC, if the template has one
    tz: Option<i64>,
    /// the digits of a fractional second the template asked for (`FF3`), or 0
    ff: i64,
}

fn do_to_timestamp(text: &str, fmts: &[Fmt]) -> DR<(Tm, u32)> {
    let tmfc = from_char(fmts, text)?;
    let flags = datetime_type(fmts);
    let mut tm = Tm { year: 0, mon: 1, mday: 1, hour: 0, min: 0, sec: 0, fsec: 0, tz: None, ff: tmfc.ff };
    let mut fmask = 0u32;
    const YEAR: u32 = 1;
    const MONTH: u32 = 2;
    const DAY: u32 = 4;
    const DATE_M: u32 = YEAR | MONTH | DAY;
    let (mut cc, mut ddd, mut dd) = (tmfc.cc, tmfc.ddd, tmfc.dd);
    if tmfc.ssss != 0 {
        let mut x = tmfc.ssss;
        tm.hour = x / 3600;
        x %= 3600;
        tm.min = x / 60;
        x %= 60;
        tm.sec = x;
    }
    if tmfc.ss != 0 {
        tm.sec = tmfc.ss;
    }
    if tmfc.mi != 0 {
        tm.min = tmfc.mi;
    }
    if tmfc.hh != 0 {
        tm.hour = tmfc.hh;
    }
    if tmfc.clock_12_hour != 0 {
        if tm.hour < 1 || tm.hour > 12 {
            return Err(fmt_err(format!("hour \"{}\" is invalid for the 12-hour clock", tm.hour)));
        }
        if tmfc.pm != 0 && tm.hour < 12 {
            tm.hour += 12;
        } else if tmfc.pm == 0 && tm.hour == 12 {
            tm.hour = 0;
        }
    }
    if tmfc.year != 0 {
        // with a century and a year of one or two digits, the year within it
        if cc != 0 && tmfc.yysz <= 2 {
            if tmfc.bc != 0 {
                cc = -cc;
            }
            tm.year = tmfc.year % 100;
            if tm.year != 0 {
                if cc >= 0 {
                    tm.year += (cc - 1) * 100;
                } else {
                    tm.year = (cc + 1) * 100 - tm.year + 1;
                }
            } else {
                tm.year = cc * 100 + if cc >= 0 { 0 } else { 1 };
            }
        } else {
            tm.year = tmfc.year;
            if tmfc.bc != 0 {
                tm.year = -tm.year;
            }
            // our representation of BC years
            if tm.year < 0 {
                tm.year += 1;
            }
        }
        fmask |= YEAR;
    } else if cc != 0 {
        // the first year of the century
        if tmfc.bc != 0 {
            cc = -cc;
        }
        tm.year = if cc >= 0 { (cc - 1) * 100 + 1 } else { cc * 100 + 1 };
        fmask |= YEAR;
    }
    if tmfc.j != 0 {
        let (y, m, d) = j2date(tmfc.j);
        tm.year = y;
        tm.mon = m;
        tm.mday = d;
    }
    if tmfc.ww != 0 {
        if tmfc.mode == Some(Mode::IsoWeek) {
            // the date is the start of the ISO week (a Monday) unless the day of the week is given
            let jday = if tmfc.d != 0 {
                let j = isoweek2j(tm.year, tmfc.ww);
                if tmfc.d > 1 { j + tmfc.d - 2 } else { j + 6 }
            } else {
                isoweek2j(tm.year, tmfc.ww)
            };
            let (y, m, d) = j2date(jday);
            tm.year = y;
            tm.mon = m;
            tm.mday = d;
            fmask |= DATE_M;
        } else {
            ddd = (tmfc.ww - 1) * 7 + 1;
        }
    }
    if tmfc.w != 0 {
        dd = (tmfc.w - 1) * 7 + 1;
    }
    if dd != 0 {
        tm.mday = dd;
        fmask |= DAY;
    }
    if tmfc.mm != 0 {
        tm.mon = tmfc.mm;
        fmask |= MONTH;
    }
    if ddd != 0 && (tm.mon <= 1 || tm.mday <= 1) {
        // the month and day are not set: the day of the year makes them
        if tm.year == 0 && tmfc.bc == 0 {
            return Err(fmt_err("cannot calculate day of year without year information"));
        }
        if tmfc.mode == Some(Mode::IsoWeek) {
            let j0 = isoweek2j(tm.year, 1) - 1;
            let (y, m, d) = j2date(j0 + ddd);
            tm.year = y;
            tm.mon = m;
            tm.mday = d;
            fmask |= DATE_M;
        } else {
            const YSUM: [[i64; 13]; 2] = [
                [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
                [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
            ];
            let y = &YSUM[usize::from(isleap(tm.year))];
            let mut i = 1;
            while i <= 12 {
                if ddd <= y[i] {
                    break;
                }
                i += 1;
            }
            if tm.mon <= 1 {
                tm.mon = i as i64;
            }
            if tm.mday <= 1 {
                tm.mday = ddd - y[i - 1];
            }
            fmask |= MONTH | DAY;
        }
    }
    if tmfc.ms != 0 {
        tm.fsec += tmfc.ms * 1000;
    }
    if tmfc.us != 0 {
        tm.fsec += tmfc.us;
    }
    // range-check the date fields
    if fmask != 0 {
        let mut over = false;
        if fmask & MONTH != 0 && (tm.mon < 1 || tm.mon > 12) {
            over = true;
        }
        if fmask & DAY != 0 && (tm.mday < 1 || tm.mday > 31) {
            over = true;
        }
        if !over && fmask & DATE_M == DATE_M {
            const DAYS_IN: [[i64; 12]; 2] =
                [[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31], [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]];
            if tm.mday > DAYS_IN[usize::from(isleap(tm.year))][(tm.mon - 1) as usize] {
                over = true;
            }
        }
        if over {
            return Err(field_overflow(text));
        }
    }
    // and the time fields
    if tm.hour < 0
        || tm.hour >= 24
        || tm.min < 0
        || tm.min >= 60
        || tm.sec < 0
        || tm.sec >= 60
        || tm.fsec < 0
        || tm.fsec >= USECS_PER_SEC
    {
        return Err(field_overflow(text));
    }
    if tmfc.tzsign != 0 {
        if tmfc.tzh < 0 || tmfc.tzh > i64::from(MAX_TZDISP_HOUR) || tmfc.tzm < 0 || tmfc.tzm >= 60 {
            return Err(bad("22009", format!("time zone displacement out of range: \"{text}\"")));
        }
        tm.tz = Some(-tmfc.tzsign * (tmfc.tzh * 3600 + tmfc.tzm * 60));
    }
    Ok((tm, flags))
}

/// A time or timestamp rounded to `typmod` digits of fractional second, as a column of that precision
/// would hold it.
fn adjust_for_typmod(us: i64, typmod: i64) -> i64 {
    if !(0..6).contains(&typmod) {
        return us;
    }
    let scale = 10i64.pow(6 - typmod as u32);
    let offset = scale / 2;
    if us >= 0 { (us + offset) / scale * scale } else { -((-us + offset) / scale * scale) }
}

/// `tm2time`: microseconds since midnight.
fn tm2time(tm: &Tm) -> i64 {
    ((tm.hour * 60 + tm.min) * 60 + tm.sec) * USECS_PER_SEC + tm.fsec
}

fn out_of_range(what: &str, text: &str) -> PathError {
    bad("22008", format!("{what} out of range: \"{text}\""))
}

/// The date, time, or timestamp a template and a string make: parse_datetime.
pub fn parse_template(text: &str, template: &str) -> Result<Datetime, DtErr> {
    if template.is_empty() {
        // nothing to read the string by: it isn't looked at
        return Err(DtErr::Soft(fmt_err("datetime format is not dated and not timed")));
    }
    let fmts = parse_format(template)?;
    parse_fields(text, &fmts).map_err(DtErr::Soft)
}

fn parse_fields(text: &str, fmts: &[Fmt]) -> DR<Datetime> {
    let (tm, flags) = do_to_timestamp(text, fmts)?;
    if flags & DATED != 0 {
        if flags & TIMED != 0 {
            let too_big = || bad("22008", "timestamp out of range");
            if !valid_julian(tm.year, tm.mon, tm.mday) {
                return Err(too_big());
            }
            let jd = date2j(tm.year, tm.mon, tm.mday);
            let us = (jd - POSTGRES_EPOCH_JDATE)
                .checked_mul(USECS_PER_DAY)
                .and_then(|d| d.checked_add(tm2time(&tm)))
                .ok_or_else(too_big)?;
            // (a timestamp with a zone is rounded after the zone is applied)
            if let Some(tz) = tm.tz {
                // local time to UTC: the zone is seconds west
                let utc = us.checked_add(tz * USECS_PER_SEC).ok_or_else(too_big)?;
                if !(TS_MIN..TS_END).contains(&utc) {
                    return Err(too_big());
                }
                return Ok(Datetime::TimestampTz(
                    adjust_for_typmod(utc, if tm.ff == 0 { -1 } else { tm.ff }),
                    tz as i32,
                ));
            }
            // (a timestamp goes from 4714-11-24 BC to 294276-12-31 AD)
            if !(TS_MIN..TS_END).contains(&us) {
                return Err(too_big());
            }
            return Ok(Datetime::Timestamp(adjust_for_typmod(us, if tm.ff == 0 { -1 } else { tm.ff })));
        }
        if flags & ZONED != 0 {
            return Err(fmt_err("datetime format is zoned but not timed"));
        }
        if !valid_julian(tm.year, tm.mon, tm.mday) {
            return Err(out_of_range("date", text));
        }
        let jd = date2j(tm.year, tm.mon, tm.mday);
        if !(JULIAN_MIN..JULIAN_MAX).contains(&jd) {
            return Err(out_of_range("date", text));
        }
        return Ok(Datetime::Date((jd - POSTGRES_EPOCH_JDATE) as i32));
    }
    if flags & TIMED != 0 {
        let t = adjust_for_typmod(tm2time(&tm), if tm.ff == 0 { -1 } else { tm.ff });
        return Ok(match tm.tz {
            Some(tz) => Datetime::TimeTz(t, tz as i32),
            None => Datetime::Time(t),
        });
    }
    Err(fmt_err("datetime format is not dated and not timed"))
}

/// The ISO forms `.datetime()` tries, in this order.
const ISO_FORMATS: &[&str] = &[
    "yyyy-mm-dd",
    "HH24:MI:SS.USTZH:TZM",
    "HH24:MI:SS.USTZH",
    "HH24:MI:SSTZH:TZM",
    "HH24:MI:SSTZH",
    "HH24:MI:SS.US",
    "HH24:MI:SS",
    "yyyy-mm-dd HH24:MI:SS.USTZH:TZM",
    "yyyy-mm-dd HH24:MI:SS.USTZH",
    "yyyy-mm-dd HH24:MI:SSTZH:TZM",
    "yyyy-mm-dd HH24:MI:SSTZH",
    "yyyy-mm-dd HH24:MI:SS.US",
    "yyyy-mm-dd HH24:MI:SS",
    "yyyy-mm-dd\"T\"HH24:MI:SS.USTZH:TZM",
    "yyyy-mm-dd\"T\"HH24:MI:SS.USTZH",
    "yyyy-mm-dd\"T\"HH24:MI:SSTZH:TZM",
    "yyyy-mm-dd\"T\"HH24:MI:SSTZH",
    "yyyy-mm-dd\"T\"HH24:MI:SS.US",
    "yyyy-mm-dd\"T\"HH24:MI:SS",
];

/// The first of the ISO forms that reads `text`.
pub fn parse_iso(text: &str) -> Option<Datetime> {
    ISO_FORMATS.iter().find_map(|f| parse_template(text, f).ok())
}

// ---------------------------------------------------------------------------------------------
// printing and comparing

impl Datetime {
    pub fn type_name(&self) -> &'static str {
        match self {
            Datetime::Date(_) => "date",
            Datetime::Time(_) => "time without time zone",
            Datetime::TimeTz(..) => "time with time zone",
            Datetime::Timestamp(_) => "timestamp without time zone",
            Datetime::TimestampTz(..) => "timestamp with time zone",
        }
    }
}

fn split_date(days: i64) -> (i64, i64, i64) {
    j2date(days + POSTGRES_EPOCH_JDATE)
}

fn fmt_date(days: i64) -> String {
    let (y, m, d) = split_date(days);
    let (year, bc) = if y <= 0 { (1 - y, true) } else { (y, false) };
    format!("{year:04}-{m:02}-{d:02}{}", if bc { " BC" } else { "" })
}

fn fmt_time(t: i64) -> String {
    let (h, rest) = (t / (3600 * USECS_PER_SEC), t % (3600 * USECS_PER_SEC));
    let (mi, rest) = (rest / (60 * USECS_PER_SEC), rest % (60 * USECS_PER_SEC));
    let (s, frac) = (rest / USECS_PER_SEC, rest % USECS_PER_SEC);
    let mut out = format!("{h:02}:{mi:02}:{s:02}");
    if frac != 0 {
        let f = format!("{frac:06}");
        out.push('.');
        out.push_str(f.trim_end_matches('0'));
    }
    out
}

/// A zone as jsonb writes it: `+01:00`, and `+01:00:30` if it has seconds. `secs` is east.
fn fmt_zone(east: i64) -> String {
    let sign = if east < 0 { '-' } else { '+' };
    let a = east.abs();
    let (h, m, s) = (a / 3600, a % 3600 / 60, a % 60);
    if s != 0 { format!("{sign}{h:02}:{m:02}:{s:02}") } else { format!("{sign}{h:02}:{m:02}") }
}

impl fmt::Display for Datetime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Datetime::Date(d) => f.write_str(&fmt_date(i64::from(d))),
            Datetime::Time(t) => f.write_str(&fmt_time(t)),
            Datetime::TimeTz(t, west) => write!(f, "{}{}", fmt_time(t), fmt_zone(-i64::from(west))),
            Datetime::Timestamp(us) => {
                let (days, t) = (us.div_euclid(USECS_PER_DAY), us.rem_euclid(USECS_PER_DAY));
                let date = fmt_date(days);
                let (d, bc) = date.split_once(' ').map_or((date.as_str(), ""), |(d, b)| (d, b));
                write!(f, "{d}T{}{}", fmt_time(t), if bc.is_empty() { String::new() } else { format!(" {bc}") })
            }
            Datetime::TimestampTz(us, west) => {
                let east = -west;
                let local = us + i64::from(east) * USECS_PER_SEC;
                let (days, t) = (local.div_euclid(USECS_PER_DAY), local.rem_euclid(USECS_PER_DAY));
                let date = fmt_date(days);
                let (d, bc) = date.split_once(' ').map_or((date.as_str(), ""), |(d, b)| (d, b));
                write!(
                    f,
                    "{d}T{}{}{}",
                    fmt_time(t),
                    fmt_zone(i64::from(east)),
                    if bc.is_empty() { String::new() } else { format!(" {bc}") }
                )
            }
        }
    }
}

/// `checkTimezoneIsUsedForCast`.
fn needs_tz(tz: bool, from: &str, to: &str) -> DR<()> {
    if tz {
        Ok(())
    } else {
        Err(bad(
            "0A000",
            format!(
                "cannot convert value from {from} to {to} without time zone usage\u{2}Use *_tz() function for time \
                 zone support."
            ),
        ))
    }
}

/// The session's offset east of UTC, in microseconds.
fn session_offset() -> i64 {
    i64::from(super::super::time::ZONE.get()) * USECS_PER_SEC
}

/// How `a` and `b` order, if they can be compared at all (None if not: a date and a time).
pub fn compare(a: &Datetime, b: &Datetime, tz: bool) -> DR<Option<Ordering>> {
    use Datetime::*;
    // a date as a timestamp, at midnight
    let date_ts = |d: i32| i64::from(d).checked_mul(USECS_PER_DAY);
    let ord = |x: i64, y: i64| Some(x.cmp(&y));
    Ok(match (*a, *b) {
        (Date(x), Date(y)) => Some(x.cmp(&y)),
        (Time(x), Time(y)) => Some(x.cmp(&y)),
        (TimeTz(x, zx), TimeTz(y, zy)) => {
            let (tx, ty) = (x + i64::from(zx) * USECS_PER_SEC, y + i64::from(zy) * USECS_PER_SEC);
            Some(tx.cmp(&ty).then(zx.cmp(&zy)))
        }
        (Timestamp(x), Timestamp(y)) | (TimestampTz(x, _), TimestampTz(y, _)) => Some(x.cmp(&y)),
        (Date(x), Timestamp(y)) => match date_ts(x) {
            Some(t) => ord(t, y),
            None => Some(Ordering::Greater),
        },
        (Timestamp(x), Date(y)) => match date_ts(y) {
            Some(t) => ord(x, t),
            None => Some(Ordering::Less),
        },
        (Date(x), TimestampTz(y, _)) => {
            needs_tz(tz, "date", "timestamptz")?;
            match date_ts(x) {
                Some(t) => ord(t - session_offset(), y),
                None => Some(Ordering::Greater),
            }
        }
        (TimestampTz(x, _), Date(y)) => {
            needs_tz(tz, "date", "timestamptz")?;
            match date_ts(y) {
                Some(t) => ord(x, t - session_offset()),
                None => Some(Ordering::Less),
            }
        }
        (Timestamp(x), TimestampTz(y, _)) => {
            needs_tz(tz, "timestamp", "timestamptz")?;
            ord(x - session_offset(), y)
        }
        (TimestampTz(x, _), Timestamp(y)) => {
            needs_tz(tz, "timestamp", "timestamptz")?;
            ord(x, y - session_offset())
        }
        (Time(x), TimeTz(y, zy)) => {
            needs_tz(tz, "time", "timetz")?;
            let zx = -(session_offset() / USECS_PER_SEC) as i32;
            let (tx, ty) = (x + i64::from(zx) * USECS_PER_SEC, y + i64::from(zy) * USECS_PER_SEC);
            Some(tx.cmp(&ty).then(zx.cmp(&zy)))
        }
        (TimeTz(x, zx), Time(y)) => {
            needs_tz(tz, "time", "timetz")?;
            let zy = -(session_offset() / USECS_PER_SEC) as i32;
            let (tx, ty) = (x + i64::from(zx) * USECS_PER_SEC, y + i64::from(zy) * USECS_PER_SEC);
            Some(tx.cmp(&ty).then(zx.cmp(&zy)))
        }
        _ => None,
    })
}

#[allow(dead_code)]
const _: i64 = SECS_PER_DAY + ROMAN.len() as i64;
