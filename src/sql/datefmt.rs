//! `to_date(text, format)` and `to_timestamp(text, format)`: reading a date and time out of text
//! by a template, as Postgres's `DCH_from_char` and `do_to_timestamp` do.
//!
//! A template is made of patterns (`YYYY`, `MM`, `DD`, `HH24`, `MI`, `SS`, `MS`, `US`, `AM`, `Mon`,
//! `Month`, ...), quoted text, and separators. A space in it, or a punctuation mark, takes what
//! whitespace and punctuation the text has there; a number is read up to as many digits as its
//! pattern is wide, unless the next thing in the template is not a number, when it takes them
//! all; text that runs out ends the reading, and text left over is ignored.

use super::time;

/// Why the text doesn't fit the template: the SQLSTATE and message.
pub struct Fail {
    pub code: &'static str,
    pub message: String,
}

fn fail<T>(code: &'static str, message: impl Into<String>) -> Result<T, Fail> {
    Err(Fail { code, message: message.into() })
}

const FORMAT: &str = "22007"; // invalid_datetime_format
const RANGE: &str = "22008"; // datetime_field_overflow
const UNSUPPORTED: &str = "0A000";

/// What a pattern reads.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Key {
    Year4,
    Year3,
    Year2,
    Year1,
    Month,
    Day,
    DayOfYear,
    Hour12,
    Hour24,
    Minute,
    Second,
    Millis,
    Micros,
    /// FF1..FF6: fractions of a second of that many digits
    Fraction(usize),
    SecondsOfDay,
    Quarter,
    DayOfWeek,
    WeekOfMonth,
    WeekOfYear,
    IsoYear(usize),
    IsoWeek,
    IsoDay,
    Century,
    Julian,
    MonthName,
    MonthAbbr,
    DayName,
    DayAbbr,
    Meridiem,
    Era,
    TzHour,
    TzMinute,
    /// `Y,YYY`: the year with a comma after its thousands
    YearComma,
    /// `RM`: the month in Roman numerals
    RomanMonth,
    /// `IDDD`: the day of the ISO week-numbering year
    IsoDayOfYear,
    /// `TZ`, `OF`: only to_char has them
    OnlyToChar(&'static str),
}

/// A piece of a template.
enum Node {
    /// a pattern, with FM (read as many digits as there are) and TH (skip `st`, `nd`, ... after it), and its name as
    /// the template spells it (`Mon`, `MON`), which errors give
    Key { key: Key, fill: bool, th: bool, name: String },
    /// whitespace in the template
    Space,
    /// punctuation in the template
    Sep,
    /// a character that stands for itself (quoted, or a letter that is no pattern)
    Char(char),
}

const PATTERNS: &[(&str, Key)] = &[
    ("HH24", Key::Hour24),
    ("HH12", Key::Hour12),
    ("HH", Key::Hour12),
    ("IDDD", Key::IsoDayOfYear),
    ("IYYY", Key::IsoYear(4)),
    ("IYY", Key::IsoYear(3)),
    ("IY", Key::IsoYear(2)),
    ("IW", Key::IsoWeek),
    ("ID", Key::IsoDay),
    ("I", Key::IsoYear(1)),
    ("MONTH", Key::MonthName),
    ("MON", Key::MonthAbbr),
    ("MI", Key::Minute),
    ("MM", Key::Month),
    ("MS", Key::Millis),
    ("SSSSS", Key::SecondsOfDay),
    ("SSSS", Key::SecondsOfDay),
    ("SS", Key::Second),
    ("FF1", Key::Fraction(1)),
    ("FF2", Key::Fraction(2)),
    ("FF3", Key::Fraction(3)),
    ("FF4", Key::Fraction(4)),
    ("FF5", Key::Fraction(5)),
    ("FF6", Key::Fraction(6)),
    ("US", Key::Micros),
    ("Y,YYY", Key::YearComma),
    ("YYYY", Key::Year4),
    ("YYY", Key::Year3),
    ("YY", Key::Year2),
    ("Y", Key::Year1),
    ("DDD", Key::DayOfYear),
    ("DAY", Key::DayName),
    ("DD", Key::Day),
    ("DY", Key::DayAbbr),
    ("D", Key::DayOfWeek),
    ("WW", Key::WeekOfYear),
    ("W", Key::WeekOfMonth),
    ("CC", Key::Century),
    ("J", Key::Julian),
    ("Q", Key::Quarter),
    ("RM", Key::RomanMonth),
    ("A.M.", Key::Meridiem),
    ("P.M.", Key::Meridiem),
    ("AM", Key::Meridiem),
    ("PM", Key::Meridiem),
    ("A.D.", Key::Era),
    ("B.C.", Key::Era),
    ("AD", Key::Era),
    ("BC", Key::Era),
    ("TZH", Key::TzHour),
    ("TZM", Key::TzMinute),
    ("TZ", Key::OnlyToChar("TZ")),
    ("OF", Key::OnlyToChar("OF")),
];

/// The template's nodes. `FX` (fixed format) is read and has no effect, as this reading is already
/// as strict about widths as Postgres's is; `FM` and a `TH` after a pattern belong to that pattern.
fn nodes(fmt: &str) -> Result<Vec<Node>, Fail> {
    let mut out = vec![];
    let (mut rest, mut fill) = (fmt, false);
    while let Some(c) = rest.chars().next() {
        let head = |p: &str| rest.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p));
        if head("FX") {
            rest = &rest[2..];
        } else if head("FM") {
            (rest, fill) = (&rest[2..], true);
        } else if c == '"' {
            // quoted text: each character stands for one of the text's (a backslash quotes the next one, `\"`)
            let mut chars = rest[1..].chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '"' => break,
                    '\\' => {
                        let mut ahead = chars.clone();
                        match ahead.next() {
                            Some(next) => {
                                out.push(Node::Char(next));
                                chars = ahead;
                            }
                            None => out.push(Node::Char('\\')),
                        }
                    }
                    ch => out.push(Node::Char(ch)),
                }
            }
            rest = chars.as_str();
        } else if c == '\\' && rest[1..].starts_with('"') {
            // (outside quotes a backslash is special only before a double quote)
            out.push(Node::Char('"'));
            rest = &rest[2..];
        } else if let Some((p, key)) = PATTERNS.iter().find(|(p, _)| head(p)) {
            let name = rest[..p.len()].to_string();
            if let Key::OnlyToChar(_) = key {
                return fail(UNSUPPORTED, format!("formatting field \"{name}\" is only supported in to_char"));
            }
            rest = &rest[p.len()..];
            let th = rest.get(..2).is_some_and(|h| h.eq_ignore_ascii_case("TH"));
            if th {
                rest = &rest[2..];
            }
            out.push(Node::Key { key: *key, fill: std::mem::take(&mut fill), th, name });
        } else {
            rest = &rest[c.len_utf8()..];
            out.push(if c.is_whitespace() {
                Node::Space
            } else if c.is_alphanumeric() {
                Node::Char(c)
            } else {
                Node::Sep
            });
        }
    }
    Ok(out)
}

/// What was read.
#[derive(Default)]
struct Fields {
    year: Option<i64>,
    /// how many digits the year pattern was
    year_digits: usize,
    bc: bool,
    month: i64,
    day: i64,
    day_of_year: i64,
    hour: i64,
    twelve: bool,
    pm: bool,
    minute: i64,
    second: i64,
    millis: i64,
    micros: i64,
    seconds_of_day: i64,
    week_of_month: i64,
    week_of_year: i64,
    iso_week: i64,
    iso_day: i64,
    century: i64,
    julian: i64,
    tz_sign: Option<i64>,
    tz_hour: i64,
    tz_minute: i64,
    /// whether the template's date is a Gregorian one (`YYYY`, `MM`, `DD`, ...) or an ISO week date (`IYYY`, `IW`,
    /// `ID`, `IDDD`), once a pattern has said: the two don't mix
    iso: Option<bool>,
    /// FF1..FF6's digits, which the timestamp is rounded to
    fprec: Option<u32>,
}

/// What `to_date` and `to_timestamp` make of it.
pub struct Parsed {
    /// days since 1970-01-01
    pub days: i32,
    /// the time of day, in microseconds
    pub micros: i64,
    /// the offset it names, in seconds east of UTC
    pub tz: Option<i64>,
    /// the fraction's digits an FFn pattern read (to_timestamp rounds to them)
    pub fprec: Option<u32>,
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

/// The names one of which starts the text (in any case): the index and the length matched.
fn seq_search(s: &str, names: &[&str], len: Option<usize>) -> Option<(usize, usize)> {
    names.iter().enumerate().find_map(|(i, n)| {
        let n = &n[..len.unwrap_or(n.len())];
        let head = s.get(..n.len())?;
        head.eq_ignore_ascii_case(n).then_some((i, n.len()))
    })
}

/// Postgres's `adjust_partial_year_to_2020`: a year of fewer than 4 digits, moved toward 2020.
fn adjust(year: i64) -> i64 {
    match year {
        y if y < 70 => y + 2000,
        y if y < 100 => y + 1900,
        y if y < 520 => y + 2000,
        y if y < 1000 => y + 1000,
        y => y,
    }
}

/// Reads a whole number by `strtol`'s rules (a sign, then digits) from the text. Not `slurp`ing, it
/// looks at only the first `width` characters. Returns the value and how many characters it took.
fn number(s: &mut &str, width: usize, slurp: bool, name: &str) -> Result<(i64, usize), Fail> {
    let window: &str = if slurp {
        s
    } else {
        let end = s.char_indices().nth(width).map_or(s.len(), |(i, _)| i);
        &s[..end]
    };
    // (a fixed-width field the text has fewer characters left for)
    let remain = window.chars().count();
    if !slurp && remain < width {
        return fail(
            FORMAT,
            format!(
                "source string too short for \"{name}\" formatting field: Field requires {width} characters, but only {remain} remain. Hint: If your source string is not fixed-width, try using the \"FM\" modifier."
            ),
        );
    }
    let mut chars = window.char_indices().peekable();
    let sign = match chars.peek() {
        Some((_, '-')) => {
            chars.next();
            -1
        }
        Some((_, '+')) => {
            chars.next();
            1
        }
        _ => 1,
    };
    let mut value: i64 = 0;
    let mut end = 0;
    let mut digits = 0;
    while let Some(&(i, c)) = chars.peek() {
        let Some(d) = c.to_digit(10) else { break };
        value = value.saturating_mul(10).saturating_add(d as i64);
        end = i + 1;
        digits += 1;
        chars.next();
    }
    if digits == 0 {
        let shown: String = s.chars().take(width.max(1)).collect();
        return fail(FORMAT, format!("invalid value \"{shown}\" for \"{name}\": Value must be an integer."));
    }
    if value > i32::MAX as i64 {
        return fail(
            RANGE,
            format!(
                "value for \"{name}\" in source string is out of range: Value must be in the range -2147483648 to \
                 2147483647."
            ),
        );
    }
    let used = window[..end].chars().count();
    // a full window that is not all number: the field is fixed-width and the text isn't
    if !slurp && window.chars().count() == width && used < width {
        return fail(
            FORMAT,
            format!(
                "invalid value \"{window}\" for \"{name}\": Field requires {width} characters, but only {used} could be parsed. Hint: If your source string is not fixed-width, try using the \"FM\" modifier."
            ),
        );
    }
    *s = &s[end..];
    Ok((sign * value, used))
}

/// Reads `input` by the template `fmt`.
pub fn parse(input: &str, fmt: &str) -> Result<Parsed, Fail> {
    let nodes = nodes(fmt)?;
    let mut f = Fields::default();
    let mut s = input;
    // (how many characters of the text were skipped past what the template's spaces and separators took, as
    // DCH_from_char counts them: a character of the template then takes none, and a TZH may take a skipped `-`)
    let mut extra_skip: i64 = 0;
    for (at, node) in nodes.iter().enumerate() {
        if s.is_empty() {
            break;
        }
        // whitespace is skipped at the start of the text and before each field
        if at == 0 || matches!(node, Node::Key { .. }) {
            let trimmed = s.trim_start();
            extra_skip += (s.chars().count() - trimmed.chars().count()) as i64;
            s = trimmed;
            // (a field with only spaces left for it reads nothing, as DCH_from_char does: `invalid value ""`)
            if s.is_empty() && !matches!(node, Node::Key { .. }) {
                break;
            }
        }
        match node {
            Node::Space | Node::Sep => {
                // one space or separator of the template takes one of the text's, if one is there
                extra_skip -= 1;
                if let Some(c) = s.chars().next().filter(|c| c.is_whitespace() || is_separator_char(*c)) {
                    s = &s[c.len_utf8()..];
                    extra_skip += 1;
                }
            }
            Node::Char(_) => {
                // (one skipped already stands for it)
                if extra_skip > 0 {
                    extra_skip -= 1;
                } else {
                    let c = s.chars().next().map_or(0, char::len_utf8);
                    s = &s[c..];
                }
            }
            Node::Key { key, fill, th, name } => {
                if *key == Key::TzHour && !s.starts_with(['+', '-', ' ']) {
                    // with no sign of its own, a `-` skipped before it is the offset's
                    let before = &input[..input.len() - s.len()];
                    f.tz_sign = Some(if extra_skip > 0 && before.ends_with('-') { -1 } else { 1 });
                }
                // digits fill the pattern's width unless the next pattern is no number, or it is FM
                let slurp = *fill
                    || match nodes.get(at + 1) {
                        None => true,
                        Some(Node::Key { key, .. }) => !is_number(*key),
                        Some(Node::Char(c)) => !c.is_ascii_digit(),
                        Some(_) => true,
                    }
                    || *th;
                read(&mut f, *key, name, &mut s, slurp)?;
                if *th {
                    // (the suffix's two characters, whatever they are)
                    for _ in 0..2 {
                        s = &s[s.chars().next().map_or(0, char::len_utf8)..];
                    }
                }
                // the whitespace after a field is skipped, and is all that counts as skipped now
                let trimmed = s.trim_start();
                extra_skip = (s.chars().count() - trimmed.chars().count()) as i64;
                s = trimmed;
            }
        }
    }
    finish(f, input)
}

/// A printable ASCII character that is no letter or digit: what a template's separator takes (is_separator_char).
fn is_separator_char(c: char) -> bool {
    c.is_ascii_graphic() && !c.is_ascii_alphanumeric()
}

fn is_number(k: Key) -> bool {
    !matches!(
        k,
        Key::MonthName
            | Key::MonthAbbr
            | Key::RomanMonth
            | Key::DayName
            | Key::DayAbbr
            | Key::Meridiem
            | Key::Era
            | Key::OnlyToChar(_)
    )
}

/// Sets the year (moved toward 2020 if `adjusted`), or refuses a second one that differs.
fn year_of(f: &mut Fields, v: i64, width: usize, adjusted: bool) -> Result<(), Fail> {
    if f.year.is_some_and(|y| y != 0 && y != v) {
        return fail(
            FORMAT,
            "conflicting values for \"YYYY\" field in formatting string: This value contradicts a previous setting for the same field type.",
        );
    }
    (f.year, f.year_digits) = (Some(if adjusted { adjust(v) } else { v }), width);
    Ok(())
}

/// Reads one pattern's field.
fn read(f: &mut Fields, key: Key, name: &str, s: &mut &str, slurp: bool) -> Result<(), Fail> {
    let conflict = |what: &str| -> Result<(), Fail> {
        fail(
            FORMAT,
            format!(
                "conflicting values for \"{what}\" field in formatting string: This value contradicts a previous setting for the same field type."
            ),
        )
    };
    macro_rules! set {
        ($field:expr, $v:expr) => {{
            let v = $v;
            if $field != 0 && $field != v {
                return conflict(name);
            }
            $field = v;
        }};
    }
    // a Gregorian date's patterns and an ISO week date's don't mix (from_char_set_mode)
    let iso = match key {
        Key::IsoYear(_) | Key::IsoWeek | Key::IsoDay | Key::IsoDayOfYear => Some(true),
        Key::Year4
        | Key::Year3
        | Key::Year2
        | Key::Year1
        | Key::YearComma
        | Key::Month
        | Key::MonthName
        | Key::MonthAbbr
        | Key::RomanMonth
        | Key::Day
        | Key::DayOfYear
        | Key::DayOfWeek
        | Key::WeekOfYear
        | Key::WeekOfMonth => Some(false),
        _ => None,
    };
    match (f.iso, iso) {
        (Some(was), Some(now)) if was != now => {
            return fail(
                FORMAT,
                "invalid combination of date conventions Hint: Do not mix Gregorian and ISO week date conventions in a formatting template.",
            );
        }
        (None, Some(now)) => f.iso = Some(now),
        _ => {}
    }
    // (a name's text, for an error: up to the next whitespace)
    let word = |s: &str| s.split(char::is_whitespace).next().unwrap_or_default().to_string();
    match key {
        Key::YearComma => {
            // `1,582`: as sscanf's `%d,%03d` reads it
            let bad = || Fail { code: FORMAT, message: "invalid input string for \"Y,YYY\"".into() };
            let mut rest = s.trim_start();
            let (neg, unsigned) = match rest.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, rest.strip_prefix('+').unwrap_or(rest)),
            };
            let n = unsigned.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 || !unsigned[n..].starts_with(',') {
                return Err(bad());
            }
            let millennia: i64 = unsigned[..n].parse().map_err(|_| bad())?;
            rest = unsigned[n + 1..].trim_start();
            let m = rest.bytes().take(3).take_while(u8::is_ascii_digit).count();
            if m == 0 {
                return Err(bad());
            }
            let years: i64 = rest[..m].parse().map_err(|_| bad())?;
            *s = &rest[m..];
            let millennia = if neg { -millennia } else { millennia };
            year_of(f, millennia * 1000 + years, 4, false)?;
        }
        Key::RomanMonth => {
            const ROMAN: [&str; 12] = ["XII", "XI", "X", "IX", "VIII", "VII", "VI", "V", "IV", "III", "II", "I"];
            let Some((i, n)) = seq_search(s, &ROMAN, None) else {
                return fail(
                    FORMAT,
                    format!(
                        "invalid value \"{}\" for \"{name}\": The given value did not match any of the allowed values for this field.",
                        word(s)
                    ),
                );
            };
            set!(f.month, 12 - i as i64);
            *s = &s[n..];
        }
        // (the day of the ISO year, which `finish` counts from the Monday of its first week)
        Key::IsoDayOfYear => set!(f.day_of_year, number(s, 3, slurp, name)?.0),
        Key::Year4 => {
            let (v, _) = number(s, 4, slurp, name)?;
            year_of(f, v, 4, false)?;
        }
        Key::Year3 | Key::Year2 | Key::Year1 => {
            let width = match key {
                Key::Year3 => 3,
                Key::Year2 => 2,
                _ => 1,
            };
            let (v, used) = number(s, width, slurp, name)?;
            year_of(f, v, width, used < 4)?;
        }
        // (the year, read with the ISO week's patterns: IYYY as it stands, the shorter ones moved toward 2020)
        Key::IsoYear(width) => {
            let (v, _) = number(s, width, slurp, name)?;
            year_of(f, v, width, width != 4)?;
        }
        Key::Month => set!(f.month, number(s, 2, slurp, name)?.0),
        Key::Day => set!(f.day, number(s, 2, slurp, name)?.0),
        Key::DayOfYear => set!(f.day_of_year, number(s, 3, slurp, name)?.0),
        Key::Hour12 => {
            f.twelve = true;
            set!(f.hour, number(s, 2, slurp, name)?.0);
        }
        Key::Hour24 => set!(f.hour, number(s, 2, slurp, name)?.0),
        Key::Minute => set!(f.minute, number(s, 2, slurp, name)?.0),
        Key::Second => set!(f.second, number(s, 2, slurp, name)?.0),
        Key::Millis => {
            let (v, used) = number(s, 3, slurp, name)?;
            // 25 is 250 ms, not 25
            set!(f.millis, v * [1, 100, 10, 1][used.min(3)]);
        }
        Key::Micros | Key::Fraction(_) => {
            let width = if let Key::Fraction(n) = key { n } else { 6 };
            if let Key::Fraction(n) = key {
                f.fprec = Some(n as u32);
            }
            let (v, used) = number(s, width, slurp, name)?;
            // 25 is 250000 µs
            let scaled = if used <= 6 { v * 10i64.pow(6 - used as u32) } else { v };
            set!(f.micros, scaled);
        }
        Key::SecondsOfDay => set!(f.seconds_of_day, number(s, 5, slurp, name)?.0),
        Key::Quarter | Key::DayOfWeek => {
            number(s, 1, slurp, name)?;
        }
        Key::WeekOfMonth => set!(f.week_of_month, number(s, 1, slurp, name)?.0),
        Key::WeekOfYear => set!(f.week_of_year, number(s, 2, slurp, name)?.0),
        Key::IsoWeek => set!(f.iso_week, number(s, 2, slurp, name)?.0),
        Key::IsoDay => set!(f.iso_day, number(s, 1, slurp, name)?.0),
        Key::Century => set!(f.century, number(s, 2, slurp, name)?.0),
        Key::Julian => set!(f.julian, number(s, 7, slurp, name)?.0),
        Key::MonthName | Key::MonthAbbr => {
            let abbr = key == Key::MonthAbbr;
            let Some((i, n)) = seq_search(s, &MONTHS, abbr.then_some(3)) else {
                let shown = word(s);
                return fail(
                    FORMAT,
                    format!(
                        "invalid value \"{shown}\" for \"{name}\": The given value did not match any of the allowed values for this field."
                    ),
                );
            };
            set!(f.month, i as i64 + 1);
            *s = &s[n..];
        }
        Key::DayName | Key::DayAbbr => {
            let Some((_, n)) = seq_search(s, &DAYS, (key == Key::DayAbbr).then_some(3)) else {
                let shown = word(s);
                return fail(
                    FORMAT,
                    format!(
                        "invalid value \"{shown}\" for \"{name}\": The given value did not match any of the allowed values for this field."
                    ),
                );
            };
            *s = &s[n..];
        }
        Key::Meridiem => {
            let Some((i, n)) = seq_search(s, &["am", "pm", "a.m.", "p.m."], None) else {
                let shown = word(s);
                return fail(
                    FORMAT,
                    format!(
                        "invalid value \"{shown}\" for \"{name}\": The given value did not match any of the allowed values for this field."
                    ),
                );
            };
            (f.pm, f.twelve) = (i % 2 == 1, true);
            *s = &s[n..];
        }
        Key::Era => {
            let Some((i, n)) = seq_search(s, &["ad", "bc", "a.d.", "b.c."], None) else {
                let shown = word(s);
                return fail(
                    FORMAT,
                    format!(
                        "invalid value \"{shown}\" for \"{name}\": The given value did not match any of the allowed values for this field."
                    ),
                );
            };
            f.bc = i % 2 == 1;
            *s = &s[n..];
        }
        Key::TzHour => {
            // an optional sign, east positive, then the hours (with none, the sign `parse` found before it)
            f.tz_sign = Some(match s.chars().next() {
                Some('-') => {
                    *s = &s[1..];
                    -1
                }
                Some('+' | ' ') => {
                    *s = &s[1..];
                    1
                }
                _ => f.tz_sign.unwrap_or(1),
            });
            f.tz_hour = number(s, 2, slurp, name)?.0;
        }
        Key::TzMinute => {
            // (a TZM alone is east of UTC)
            f.tz_sign.get_or_insert(1);
            f.tz_minute = number(s, 2, slurp, name)?.0;
        }
        Key::OnlyToChar(n) => {
            return fail(UNSUPPORTED, format!("formatting field \"{n}\" is only supported in to_char"));
        }
    }
    Ok(())
}

/// The fields as a date and time of day (Postgres's `do_to_timestamp`).
fn finish(f: Fields, input: &str) -> Result<Parsed, Fail> {
    let out_of_range = || Fail { code: RANGE, message: format!("date/time field value out of range: \"{input}\"") };
    let mut year: i64 = 0;
    if let Some(y) = f.year.filter(|y| *y != 0) {
        year = if f.century != 0 && f.year_digits <= 2 {
            // the century and the last two digits
            let cc = if f.bc { -f.century } else { f.century };
            let yy = y % 100;
            if yy != 0 {
                if cc >= 0 { yy + (cc - 1) * 100 } else { (cc + 1) * 100 - yy + 1 }
            } else {
                cc * 100 + i64::from(cc < 0)
            }
        } else {
            let y = if f.bc { -y } else { y };
            if y < 0 { y + 1 } else { y }
        };
    } else if f.century != 0 {
        let cc = if f.bc { -f.century } else { f.century };
        year = if cc >= 0 { (cc - 1) * 100 + 1 } else { cc * 100 + 1 };
    }
    let (mut month, mut day) = (1, 1);
    if f.month != 0 {
        month = f.month;
    }
    if f.day != 0 {
        day = f.day;
    }
    if f.week_of_month != 0 {
        day = (f.week_of_month - 1) * 7 + 1;
    }
    let mut ddd = f.day_of_year;
    if f.week_of_year != 0 && ddd == 0 {
        ddd = (f.week_of_year - 1) * 7 + 1;
    }
    let days = if f.julian != 0 {
        // the Julian day number: 2440588 is 1970-01-01
        i32::try_from(f.julian - 2_440_588).map_err(|_| out_of_range())?
    } else if f.iso_week != 0 {
        // an ISO week date: the Monday of week 1 is the one in the week of January 4th
        let jan4 = time::days_from_civil(year, 1, 4);
        let monday = jan4 - (jan4 + 3).rem_euclid(7);
        let (week, wday) = (f.iso_week.max(1), f.iso_day.max(1));
        i32::try_from(monday + (week - 1) * 7 + (wday - 1)).map_err(|_| out_of_range())?
    } else if f.iso == Some(true) && ddd != 0 {
        // IDDD: the day of the ISO year, from the Monday of its first week
        if year == 0 && !f.bc {
            return fail(FORMAT, "cannot calculate day of year without year information");
        }
        let jan4 = time::days_from_civil(year, 1, 4);
        let monday = jan4 - (jan4 + 3).rem_euclid(7);
        i32::try_from(monday + ddd - 1).map_err(|_| out_of_range())?
    } else {
        if ddd != 0 && (month <= 1 || day <= 1) {
            if year == 0 && !f.bc {
                return fail(FORMAT, "cannot calculate day of year without year information");
            }
            let in_year = if time::days_in_month(year, 2) == 29 { 366 } else { 365 };
            if !(1..=in_year).contains(&ddd) {
                return Err(out_of_range());
            }
            let first = time::days_from_civil(year, 1, 1);
            let (y, m, d) = time::civil_from_days(first + ddd - 1);
            (year, month, day) = (y, m as i64, d as i64);
        }
        if !(1..=12).contains(&month) || day < 1 || day > time::days_in_month(year, month as u32) as i64 {
            return Err(out_of_range());
        }
        let days = time::days_from_civil(year, month as u32, day as u32);
        // Postgres's dates: 4714-11-24 BC to 5874897-12-31
        if !(-2_440_588..=2_145_042_905).contains(&days) {
            return fail(RANGE, format!("date out of range: \"{input}\""));
        }
        days as i32
    };
    // the time of day
    let (mut hour, mut minute, mut second) = (f.hour, f.minute, f.second);
    if f.seconds_of_day != 0 {
        (hour, minute, second) = (f.seconds_of_day / 3600, f.seconds_of_day % 3600 / 60, f.seconds_of_day % 60);
    }
    if f.twelve {
        if !(1..=12).contains(&hour) {
            return fail(
                RANGE,
                format!(
                    "hour \"{hour}\" is invalid for the 12-hour clock. Hint: Use the 24-hour clock, or give an hour between 1 and 12."
                ),
            );
        }
        if f.pm && hour < 12 {
            hour += 12;
        } else if !f.pm && hour == 12 {
            hour = 0;
        }
    }
    let fraction = f.millis * 1000 + f.micros;
    if !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
        || !(0..1_000_000).contains(&fraction)
    {
        return Err(out_of_range());
    }
    let micros = ((hour * 60 + minute) * 60 + second) * 1_000_000 + fraction;
    let tz = f.tz_sign.map(|sign| sign * (f.tz_hour * 3600 + f.tz_minute * 60));
    Ok(Parsed { days, micros, tz, fprec: f.fprec })
}
