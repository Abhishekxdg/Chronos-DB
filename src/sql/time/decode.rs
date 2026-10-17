//! Postgres's date and time input, as datetime.c reads it: the text is cut into fields (`ParseDateTime`), which
//! are then read as a timestamp or date (`DecodeDateTime`), a time of day (`DecodeTimeOnly`) or an interval
//! (`DecodeInterval`, and ISO 8601's `P1Y2M` forms). Each step follows the C code, field by field, so the forms
//! Postgres takes (`J2452271`, `20011227T040506-08`, `allballs`, `Feb 16 17:32:01 PST`, `1 day 02:03 ago`, ...)
//! and its errors are the same here.

use super::{
    Bad, DAY, DateOrder, HOUR, Interval, IntervalStyle, MIN_TIMESTAMP, MINUTE, SECOND, TS_INF, civil_from_days,
    days_from_civil, days_in_month, now, offset_at, offset_for_local,
};

/// What a field of the text looks like (ParseDateTime's DTK_NUMBER, DTK_STRING, ...).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ft {
    Number,
    String,
    Date,
    Time,
    Tz,
    Special,
}

/// Why text isn't a date or time (datetime.c's DTERR codes).
#[derive(Debug, PartialEq)]
enum DtErr {
    BadFormat,
    FieldOverflow,
    MdFieldOverflow,
    TzDispOverflow,
    BadTimezone(String),
}

impl DtErr {
    fn bad(self) -> Bad {
        match self {
            DtErr::BadFormat => Bad::Syntax,
            DtErr::FieldOverflow => Bad::Field,
            DtErr::MdFieldOverflow => Bad::MdField,
            DtErr::TzDispOverflow => Bad::Zone,
            DtErr::BadTimezone(z) => Bad::NoZone(z),
        }
    }
}

type R<T> = Result<T, DtErr>;

const MAXDATEFIELDS: usize = 25;
const TOKMAXLEN: usize = 10;

// token types (datetime.h)
const RESERV: u32 = 0;
const MONTH: u32 = 1;
const YEAR: u32 = 2;
const DAYF: u32 = 3;
const TZ: u32 = 5;
const DTZ: u32 = 6;
const IGNORE_DTF: u32 = 8;
const AMPM: u32 = 9;
const HOURF: u32 = 10;
const MINUTEF: u32 = 11;
const SECONDF: u32 = 12;
const MILLISECOND: u32 = 13;
const MICROSECOND: u32 = 14;
const DOY: u32 = 15;
const DOW: u32 = 16;
const UNITS: u32 = 17;
const ADBC: u32 = 18;
const AGO: u32 = 19;
const ISOTIME: u32 = 23;
const WEEK: u32 = 24;
const DECADE: u32 = 25;
const CENTURY: u32 = 26;
const MILLENNIUM: u32 = 27;
const DTZMOD: u32 = 28;
const UNKNOWN_FIELD: u32 = 31;

// token values (datetime.h's DTK_ codes)
const DTK_DATE: i32 = 2;
const DTK_TIME: i32 = 3;
const DTK_EARLY: i32 = 9;
const DTK_LATE: i32 = 10;
const DTK_EPOCH: i32 = 11;
const DTK_NOW: i32 = 12;
const DTK_YESTERDAY: i32 = 13;
const DTK_TODAY: i32 = 14;
const DTK_TOMORROW: i32 = 15;
const DTK_ZULU: i32 = 16;
const DTK_SECOND: i32 = 18;
const DTK_MINUTE: i32 = 19;
const DTK_HOUR: i32 = 20;
const DTK_DAY: i32 = 21;
const DTK_WEEK: i32 = 22;
const DTK_MONTH: i32 = 23;
const DTK_QUARTER: i32 = 24;
const DTK_YEAR: i32 = 25;
const DTK_DECADE: i32 = 26;
const DTK_CENTURY: i32 = 27;
const DTK_MILLENNIUM: i32 = 28;
const DTK_MILLISEC: i32 = 29;
const DTK_MICROSEC: i32 = 30;
const DTK_JULIAN: i32 = 31;
const DTK_DOW: i32 = 32;
const DTK_DOY: i32 = 33;
const DTK_TZ_HOUR: i32 = 34;
const DTK_TZ_MINUTE: i32 = 35;
const DTK_ISOYEAR: i32 = 36;
const DTK_ISODOW: i32 = 37;
const DTK_TZ_UNIT: i32 = 4; // "timezone" as a unit (DTK_TZ)

const AM: i32 = 0;
const PM: i32 = 1;
const HR24: i32 = 2;
const BC: i32 = 1;

const fn m(t: u32) -> u32 {
    1 << t
}
const DTK_DATE_M: u32 = m(YEAR) | m(MONTH) | m(DAYF);
const DTK_ALL_SECS_M: u32 = m(SECONDF) | m(MILLISECOND) | m(MICROSECOND);
const DTK_TIME_M: u32 = m(HOURF) | m(MINUTEF) | DTK_ALL_SECS_M;

/// An interval's typmod fields (timestamp.h's INTERVAL_MASK of the token types).
pub(crate) const INTERVAL_FULL_RANGE: u32 = 0x7FFF;
pub(crate) const IM_YEAR: u32 = 1 << YEAR;
pub(crate) const IM_MONTH: u32 = 1 << MONTH;
pub(crate) const IM_DAY: u32 = 1 << DAYF;
pub(crate) const IM_HOUR: u32 = 1 << HOURF;
pub(crate) const IM_MINUTE: u32 = 1 << MINUTEF;
pub(crate) const IM_SECOND: u32 = 1 << SECONDF;

/// Date and time keywords (datetktbl): token, type, value.
const DATETK: &[(&str, u32, i32)] = &[
    ("+infinity", RESERV, DTK_LATE),
    ("-infinity", RESERV, DTK_EARLY),
    ("ad", ADBC, 0),
    ("allballs", RESERV, DTK_ZULU),
    ("am", AMPM, AM),
    ("apr", MONTH, 4),
    ("april", MONTH, 4),
    ("at", IGNORE_DTF, 0),
    ("aug", MONTH, 8),
    ("august", MONTH, 8),
    ("bc", ADBC, BC),
    ("d", UNITS, DTK_DAY),
    ("dec", MONTH, 12),
    ("december", MONTH, 12),
    ("dow", UNITS, DTK_DOW),
    ("doy", UNITS, DTK_DOY),
    ("dst", DTZMOD, 3600),
    ("epoch", RESERV, DTK_EPOCH),
    ("feb", MONTH, 2),
    ("february", MONTH, 2),
    ("fri", DOW, 5),
    ("friday", DOW, 5),
    ("h", UNITS, DTK_HOUR),
    ("infinity", RESERV, DTK_LATE),
    ("isodow", UNITS, DTK_ISODOW),
    ("isoyear", UNITS, DTK_ISOYEAR),
    ("j", UNITS, DTK_JULIAN),
    ("jan", MONTH, 1),
    ("january", MONTH, 1),
    ("jd", UNITS, DTK_JULIAN),
    ("jul", MONTH, 7),
    ("julian", UNITS, DTK_JULIAN),
    ("july", MONTH, 7),
    ("jun", MONTH, 6),
    ("june", MONTH, 6),
    ("m", UNITS, DTK_MONTH),
    ("mar", MONTH, 3),
    ("march", MONTH, 3),
    ("may", MONTH, 5),
    ("mm", UNITS, DTK_MINUTE),
    ("mon", DOW, 1),
    ("monday", DOW, 1),
    ("nov", MONTH, 11),
    ("november", MONTH, 11),
    ("now", RESERV, DTK_NOW),
    ("oct", MONTH, 10),
    ("october", MONTH, 10),
    ("on", IGNORE_DTF, 0),
    ("pm", AMPM, PM),
    ("s", UNITS, DTK_SECOND),
    ("sat", DOW, 6),
    ("saturday", DOW, 6),
    ("sep", MONTH, 9),
    ("sept", MONTH, 9),
    ("september", MONTH, 9),
    ("sun", DOW, 0),
    ("sunday", DOW, 0),
    ("t", ISOTIME, DTK_TIME),
    ("thu", DOW, 4),
    ("thur", DOW, 4),
    ("thurs", DOW, 4),
    ("thursday", DOW, 4),
    ("today", RESERV, DTK_TODAY),
    ("tomorrow", RESERV, DTK_TOMORROW),
    ("tue", DOW, 2),
    ("tues", DOW, 2),
    ("tuesday", DOW, 2),
    ("wed", DOW, 3),
    ("wednesday", DOW, 3),
    ("weds", DOW, 3),
    ("y", UNITS, DTK_YEAR),
    ("yesterday", RESERV, DTK_YESTERDAY),
];

/// Interval units (deltatktbl): token, type, value.
const DELTATK: &[(&str, u32, i32)] = &[
    ("@", IGNORE_DTF, 0),
    ("ago", AGO, 0),
    ("c", UNITS, DTK_CENTURY),
    ("cent", UNITS, DTK_CENTURY),
    ("centuries", UNITS, DTK_CENTURY),
    ("century", UNITS, DTK_CENTURY),
    ("d", UNITS, DTK_DAY),
    ("day", UNITS, DTK_DAY),
    ("days", UNITS, DTK_DAY),
    ("dec", UNITS, DTK_DECADE),
    ("decade", UNITS, DTK_DECADE),
    ("decades", UNITS, DTK_DECADE),
    ("decs", UNITS, DTK_DECADE),
    ("h", UNITS, DTK_HOUR),
    ("hour", UNITS, DTK_HOUR),
    ("hours", UNITS, DTK_HOUR),
    ("hr", UNITS, DTK_HOUR),
    ("hrs", UNITS, DTK_HOUR),
    ("m", UNITS, DTK_MINUTE),
    ("microsecon", UNITS, DTK_MICROSEC),
    ("mil", UNITS, DTK_MILLENNIUM),
    ("millennia", UNITS, DTK_MILLENNIUM),
    ("millennium", UNITS, DTK_MILLENNIUM),
    ("millisecon", UNITS, DTK_MILLISEC),
    ("mils", UNITS, DTK_MILLENNIUM),
    ("min", UNITS, DTK_MINUTE),
    ("mins", UNITS, DTK_MINUTE),
    ("minute", UNITS, DTK_MINUTE),
    ("minutes", UNITS, DTK_MINUTE),
    ("mon", UNITS, DTK_MONTH),
    ("mons", UNITS, DTK_MONTH),
    ("month", UNITS, DTK_MONTH),
    ("months", UNITS, DTK_MONTH),
    ("ms", UNITS, DTK_MILLISEC),
    ("msec", UNITS, DTK_MILLISEC),
    ("msecond", UNITS, DTK_MILLISEC),
    ("mseconds", UNITS, DTK_MILLISEC),
    ("msecs", UNITS, DTK_MILLISEC),
    ("qtr", UNITS, DTK_QUARTER),
    ("quarter", UNITS, DTK_QUARTER),
    ("s", UNITS, DTK_SECOND),
    ("sec", UNITS, DTK_SECOND),
    ("second", UNITS, DTK_SECOND),
    ("seconds", UNITS, DTK_SECOND),
    ("secs", UNITS, DTK_SECOND),
    ("timezone", UNITS, DTK_TZ_UNIT),
    ("timezone_h", UNITS, DTK_TZ_HOUR),
    ("timezone_m", UNITS, DTK_TZ_MINUTE),
    ("us", UNITS, DTK_MICROSEC),
    ("usec", UNITS, DTK_MICROSEC),
    ("usecond", UNITS, DTK_MICROSEC),
    ("useconds", UNITS, DTK_MICROSEC),
    ("usecs", UNITS, DTK_MICROSEC),
    ("w", UNITS, DTK_WEEK),
    ("week", UNITS, DTK_WEEK),
    ("weeks", UNITS, DTK_WEEK),
    ("y", UNITS, DTK_YEAR),
    ("year", UNITS, DTK_YEAR),
    ("years", UNITS, DTK_YEAR),
    ("yr", UNITS, DTK_YEAR),
    ("yrs", UNITS, DTK_YEAR),
];

/// A token of a table, matched as Postgres does on its first TOKMAXLEN characters.
fn lookup(table: &[(&str, u32, i32)], key: &str) -> Option<(u32, i32)> {
    let key = &key[..key.len().min(TOKMAXLEN)];
    table.iter().find(|(t, ..)| *t == key).map(|(_, ty, v)| (*ty, *v))
}

/// A zone abbreviation of the default set (`EST`, `PDT`): TZ or DTZ and its offset east of UTC in seconds.
fn abbrev(key: &str) -> Option<(u32, i32)> {
    super::super::tz::abbrev(key).map(|(secs, dst)| (if dst { DTZ } else { TZ }, secs))
}

/// A zone the tz database names (`America/New_York`), or one written as a POSIX rule (`CST7CDT,M4.1.0,M10.5.0`,
/// `FOO8BAR`), as Postgres's pg_tzset reads both.
pub(crate) fn zone_rules(name: &str) -> Option<std::sync::Arc<super::super::tz::Rules>> {
    super::super::tz::find(name).or_else(|| super::super::tz::from_posix(&name.to_ascii_uppercase()))
}

/// The fields of a date or time: year, month, day, hours, minutes, seconds (and the day of the year or week).
#[derive(Clone, Copy, Debug, Default)]
struct Tm {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
    yday: i64,
}

// ---------------------------------------------------------------- C library helpers

/// strtol: optional spaces and sign, then digits. (value, end); end is `at` when there are no digits.
fn strtol(b: &[u8], at: usize) -> (i128, usize) {
    let mut i = at;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let start = i;
    let mut v: i128 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = (v * 10 + i128::from(b[i] - b'0')).min(i128::from(u64::MAX) * 4);
        i += 1;
    }
    if i == start {
        return (0, at);
    }
    (if neg { -v } else { v }, i)
}

/// strtoint: as strtol, Err when the value doesn't fit an int.
fn strtoint(b: &[u8], at: usize, overflow: DtErr) -> R<(i64, usize)> {
    let (v, end) = strtol(b, at);
    if v > i128::from(i32::MAX) || v < i128::from(i32::MIN) {
        return Err(overflow);
    }
    Ok((v as i64, end))
}

/// strtoi64: as strtol, Err when the value doesn't fit an int64.
fn strtoi64(b: &[u8], at: usize) -> R<(i64, usize)> {
    let (v, end) = strtol(b, at);
    i64::try_from(v).map(|v| (v, end)).map_err(|_| DtErr::FieldOverflow)
}

/// atoi: the leading number of a string, 0 if there is none.
fn atoi(s: &str) -> i64 {
    let (v, _) = strtol(s.as_bytes(), 0);
    v.clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i64
}

/// strtod: the longest number at `at` (digits, a point, digits, an exponent). (value, end); end is `at` when
/// there is none.
fn strtod(b: &[u8], at: usize) -> (f64, usize) {
    let mut i = at;
    if matches!(b.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut any = i > digits_start;
    if b.get(i) == Some(&b'.') {
        let j = i + 1;
        let mut k = j;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        if k > j || any {
            any = any || k > j;
            i = k;
        }
    }
    if !any {
        return (0.0, at);
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut k = i + 1;
        if matches!(b.get(k), Some(b'-' | b'+')) {
            k += 1;
        }
        let ds = k;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        if k > ds {
            i = k;
        }
    }
    let text = std::str::from_utf8(&b[at..i]).unwrap_or("0");
    (text.parse::<f64>().unwrap_or(0.0), i)
}

/// ParseFraction: `.ddd` to the end of the field, as a number below 1.
fn parse_fraction(s: &str) -> R<f64> {
    if s == "." {
        return Ok(0.0);
    }
    let b = s.as_bytes();
    let (v, end) = strtod(b, 0);
    if end != b.len() || end == 0 {
        return Err(DtErr::BadFormat);
    }
    Ok(v)
}

/// ParseFractionalSecond: `.ddd` as microseconds.
fn parse_fractional_second(s: &str) -> R<i64> {
    Ok((parse_fraction(s)? * 1_000_000.0).round_ties_even() as i64)
}

fn date_order() -> DateOrder {
    super::DATE_STYLE.get().1
}

// ---------------------------------------------------------------- ParseDateTime

/// ParseDateTime: the text's fields, lower-cased, and what each looks like.
fn parse_fields(s: &str) -> R<Vec<(String, Ft)>> {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut out: Vec<(String, Ft)> = vec![];
    let lower = |c: u8| c.to_ascii_lowercase() as char;
    while i < n {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if out.len() >= MAXDATEFIELDS {
            return Err(DtErr::BadFormat);
        }
        let mut f = String::new();
        let ft;
        if c.is_ascii_digit() {
            while i < n && b[i].is_ascii_digit() {
                f.push(b[i] as char);
                i += 1;
            }
            if i < n && b[i] == b':' {
                ft = Ft::Time;
                f.push(':');
                i += 1;
                while i < n && (b[i].is_ascii_digit() || b[i] == b':' || b[i] == b'.') {
                    f.push(b[i] as char);
                    i += 1;
                }
            } else if i < n && matches!(b[i], b'-' | b'/' | b'.') {
                let delim = b[i];
                f.push(delim as char);
                i += 1;
                if i < n && b[i].is_ascii_digit() {
                    let mut t = if delim == b'.' { Ft::Number } else { Ft::Date };
                    while i < n && b[i].is_ascii_digit() {
                        f.push(b[i] as char);
                        i += 1;
                    }
                    // (the delimiters must match for a date of three fields)
                    if i < n && b[i] == delim {
                        t = Ft::Date;
                        f.push(delim as char);
                        i += 1;
                        while i < n && (b[i].is_ascii_digit() || b[i] == delim) {
                            f.push(b[i] as char);
                            i += 1;
                        }
                    }
                    ft = t;
                } else {
                    ft = Ft::Date;
                    while i < n && (b[i].is_ascii_alphanumeric() || b[i] == delim) {
                        f.push(lower(b[i]));
                        i += 1;
                    }
                }
            } else {
                ft = Ft::Number;
            }
        } else if c == b'.' {
            f.push('.');
            i += 1;
            while i < n && b[i].is_ascii_digit() {
                f.push(b[i] as char);
                i += 1;
            }
            ft = Ft::Number;
        } else if c.is_ascii_alphabetic() {
            let mut t = Ft::String;
            while i < n && b[i].is_ascii_alphabetic() {
                f.push(lower(b[i]));
                i += 1;
            }
            // a date with separators, or a zone's name with them (America/New_York, GMT+8); but not a keyword
            // followed by a number (`J2452271`)
            let is_date = if i < n && matches!(b[i], b'-' | b'/' | b'.') {
                true
            } else if i < n && (b[i] == b'+' || b[i].is_ascii_digit()) {
                lookup(DATETK, &f).is_none()
            } else {
                false
            };
            if is_date {
                t = Ft::Date;
                loop {
                    f.push(lower(b[i]));
                    i += 1;
                    if !(i < n
                        && (matches!(b[i], b'+' | b'-' | b'/' | b'_' | b'.' | b':') || b[i].is_ascii_alphanumeric()))
                    {
                        break;
                    }
                }
            }
            ft = t;
        } else if c == b'+' || c == b'-' {
            f.push(c as char);
            i += 1;
            while i < n && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < n && b[i].is_ascii_digit() {
                ft = Ft::Tz;
                f.push(b[i] as char);
                i += 1;
                while i < n && (b[i].is_ascii_digit() || matches!(b[i], b':' | b'.' | b'-')) {
                    f.push(b[i] as char);
                    i += 1;
                }
            } else if i < n && b[i].is_ascii_alphabetic() {
                ft = Ft::Special;
                while i < n && b[i].is_ascii_alphabetic() {
                    f.push(lower(b[i]));
                    i += 1;
                }
            } else {
                return Err(DtErr::BadFormat);
            }
        } else if c.is_ascii_punctuation() {
            // other punctuation only separates fields
            i += 1;
            continue;
        } else {
            return Err(DtErr::BadFormat);
        }
        out.push((f, ft));
    }
    Ok(out)
}

// ---------------------------------------------------------------- field readers

/// DecodeTimezone: `+08`, `-0800`, `+05:30`, `-04:15:30` as seconds *west* of UTC (Postgres's sign).
fn decode_timezone(s: &str) -> R<i32> {
    let b = s.as_bytes();
    if !matches!(b.first(), Some(b'+' | b'-')) {
        return Err(DtErr::BadFormat);
    }
    let (mut hr, mut cp) = strtoint(b, 1, DtErr::TzDispOverflow)?;
    let (mut min, mut sec) = (0, 0);
    if b.get(cp) == Some(&b':') {
        let (mi, c) = strtoint(b, cp + 1, DtErr::TzDispOverflow)?;
        min = mi;
        cp = c;
        if b.get(cp) == Some(&b':') {
            let (se, c) = strtoint(b, cp + 1, DtErr::TzDispOverflow)?;
            sec = se;
            cp = c;
        }
    } else if cp == b.len() && b.len() > 3 {
        min = hr % 100;
        hr /= 100;
    }
    if !(0..=15).contains(&hr) || !(0..60).contains(&min) || !(0..60).contains(&sec) {
        return Err(DtErr::TzDispOverflow);
    }
    let mut tz = ((hr * 60 + min) * 60 + sec) as i32;
    if b[0] == b'-' {
        tz = -tz;
    }
    if cp != b.len() {
        return Err(DtErr::BadFormat);
    }
    Ok(-tz)
}

/// DecodeTimeCommon: `hh:mm`, `hh:mm:ss`, `hh:mm:ss.fff`, `mm:ss.fff`: (hours, minutes, seconds, microseconds).
fn decode_time_common(s: &str, range: u32) -> R<(i64, i64, i64, i64)> {
    let b = s.as_bytes();
    let (mut hour, cp) = strtoi64(b, 0)?;
    if b.get(cp) != Some(&b':') {
        return Err(DtErr::BadFormat);
    }
    let (mut min, mut cp) = strtoint(b, cp + 1, DtErr::FieldOverflow)?;
    let (mut sec, mut fsec) = (0, 0);
    match b.get(cp) {
        None => {
            // (a MINUTE TO SECOND interval's two fields are mm:ss)
            if range == IM_MINUTE | IM_SECOND {
                if hour > i64::from(i32::MAX) || hour < i64::from(i32::MIN) {
                    return Err(DtErr::FieldOverflow);
                }
                sec = min;
                min = hour;
                hour = 0;
            }
        }
        Some(b'.') => {
            // (mm:ss.sss is always MINUTE TO SECOND)
            fsec = parse_fractional_second(&s[cp..])?;
            if hour > i64::from(i32::MAX) || hour < i64::from(i32::MIN) {
                return Err(DtErr::FieldOverflow);
            }
            sec = min;
            min = hour;
            hour = 0;
        }
        Some(b':') => {
            let (se, c) = strtoint(b, cp + 1, DtErr::FieldOverflow)?;
            sec = se;
            cp = c;
            match b.get(cp) {
                Some(b'.') => fsec = parse_fractional_second(&s[cp..])?,
                None => {}
                Some(_) => return Err(DtErr::BadFormat),
            }
        }
        Some(_) => return Err(DtErr::BadFormat),
    }
    if hour < 0 || !(0..60).contains(&min) || !(0..=60).contains(&sec) || !(0..=1_000_000).contains(&fsec) {
        return Err(DtErr::FieldOverflow);
    }
    Ok((hour, min, sec, fsec))
}

/// DecodeTime: a time with colons, into `tm`; the microseconds are returned.
fn decode_time(s: &str, tm: &mut Tm) -> R<i64> {
    let (h, mi, se, fsec) = decode_time_common(s, INTERVAL_FULL_RANGE)?;
    if h > i64::from(i32::MAX) {
        return Err(DtErr::FieldOverflow);
    }
    (tm.hour, tm.min, tm.sec) = (h, mi, se);
    Ok(fsec)
}

/// DecodeNumberField: digits run together, `20011227`, `011227`, `040506`, `0405`, `040506.789`: a date or a
/// time (`DTK_DATE` or `DTK_TIME`), by what is already known.
fn decode_number_field(
    s: &str,
    fmask: u32,
    tmask: &mut u32,
    tm: &mut Tm,
    fsec: &mut i64,
    is2digits: &mut bool,
) -> R<i32> {
    let mut s = s;
    if let Some(dot) = s.find('.') {
        let cp = &s[dot..];
        *fsec = if cp.len() == 1 {
            0
        } else {
            let (frac, _) = strtod(cp.as_bytes(), 0);
            (frac * 1_000_000.0).round_ties_even() as i64
        };
        s = &s[..dot];
    } else if fmask & DTK_DATE_M != DTK_DATE_M && s.len() >= 6 {
        // (from the end: two digits of day, two of month, the rest the year)
        *tmask = DTK_DATE_M;
        let len = s.len();
        tm.mday = atoi(&s[len - 2..]);
        tm.mon = atoi(&s[len - 4..len - 2]);
        tm.year = atoi(&s[..len - 4]);
        if len - 4 == 2 {
            *is2digits = true;
        }
        return Ok(DTK_DATE);
    }
    let len = s.len();
    if fmask & DTK_TIME_M != DTK_TIME_M {
        if len == 6 {
            *tmask = DTK_TIME_M;
            tm.sec = atoi(&s[4..]);
            tm.min = atoi(&s[2..4]);
            tm.hour = atoi(&s[..2]);
            return Ok(DTK_TIME);
        } else if len == 4 {
            *tmask = DTK_TIME_M;
            tm.sec = 0;
            tm.min = atoi(&s[2..]);
            tm.hour = atoi(&s[..2]);
            return Ok(DTK_TIME);
        }
    }
    Err(DtErr::BadFormat)
}

/// DecodeNumber: a number alone, as the year, month or day it is in context (DateStyle's order decides).
#[allow(clippy::too_many_arguments)]
fn decode_number(
    flen: usize,
    s: &str,
    have_text_month: bool,
    fmask: u32,
    tmask: &mut u32,
    tm: &mut Tm,
    fsec: &mut i64,
    is2digits: &mut bool,
) -> R<()> {
    *tmask = 0;
    let b = s.as_bytes();
    let (val, cp) = strtoint(b, 0, DtErr::FieldOverflow)?;
    if cp == 0 {
        return Err(DtErr::BadFormat);
    }
    if b.get(cp) == Some(&b'.') {
        // more than two digits before the point: a date or a run-together time (2001.360, 040506.789)
        if cp > 2 {
            decode_number_field(s, fmask | DTK_DATE_M, tmask, tm, fsec, is2digits)?;
            return Ok(());
        }
        *fsec = parse_fractional_second(&s[cp..])?;
    } else if cp != b.len() {
        return Err(DtErr::BadFormat);
    }
    // a day of the year
    if flen == 3 && fmask & DTK_DATE_M == m(YEAR) && (1..=366).contains(&val) {
        *tmask = m(DOY) | m(MONTH) | m(DAYF);
        tm.yday = val;
        return Ok(());
    }
    let order = date_order();
    match fmask & DTK_DATE_M {
        0 => {
            if flen >= 3 || order == DateOrder::Ymd {
                *tmask = m(YEAR);
                tm.year = val;
            } else if order == DateOrder::Dmy {
                *tmask = m(DAYF);
                tm.mday = val;
            } else {
                *tmask = m(MONTH);
                tm.mon = val;
            }
        }
        x if x == m(YEAR) => {
            *tmask = m(MONTH);
            tm.mon = val;
        }
        x if x == m(MONTH) => {
            if have_text_month && (flen >= 3 || order == DateOrder::Ymd) {
                *tmask = m(YEAR);
                tm.year = val;
            } else {
                *tmask = m(DAYF);
                tm.mday = val;
            }
        }
        x if x == m(YEAR) | m(MONTH) => {
            if have_text_month && flen >= 3 && *is2digits {
                // (DD-MON-YYYY: the first number was the day)
                *tmask = m(DAYF);
                tm.mday = tm.year;
                tm.year = val;
                *is2digits = false;
            } else {
                *tmask = m(DAYF);
                tm.mday = val;
            }
        }
        x if x == m(DAYF) => {
            *tmask = m(MONTH);
            tm.mon = val;
        }
        x if x == m(MONTH) | m(DAYF) => {
            *tmask = m(YEAR);
            tm.year = val;
        }
        x if x == DTK_DATE_M => {
            decode_number_field(s, fmask, tmask, tm, fsec, is2digits)?;
            return Ok(());
        }
        _ => return Err(DtErr::BadFormat),
    }
    if *tmask == m(YEAR) {
        *is2digits = flen <= 2;
    }
    Ok(())
}

/// DecodeDate: a date written with separators, `1997-02-10`, `2/10/1997`, `10.02.1997`, `Feb-10-1997`.
fn decode_date(s: &str, fmask: u32, tmask: &mut u32, is2digits: &mut bool, tm: &mut Tm) -> R<()> {
    let mut fmask = fmask;
    *tmask = 0;
    let b = s.as_bytes();
    let mut fields: Vec<Option<&str>> = vec![];
    let mut i = 0;
    while i < b.len() && fields.len() < MAXDATEFIELDS {
        while i < b.len() && !b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if i >= b.len() {
            return Err(DtErr::BadFormat);
        }
        let start = i;
        if b[i].is_ascii_digit() {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        } else {
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
        }
        fields.push(Some(&s[start..i]));
        // (the character after a field is its end, whatever it is)
        if i < b.len() {
            i += 1;
        }
    }
    let mut have_text_month = false;
    for f in fields.iter_mut() {
        let Some(text) = *f else { continue };
        if text.as_bytes()[0].is_ascii_alphabetic() {
            let (ty, val) = lookup(DATETK, text).unwrap_or((UNKNOWN_FIELD, 0));
            if ty == IGNORE_DTF {
                continue;
            }
            if ty != MONTH {
                return Err(DtErr::BadFormat);
            }
            tm.mon = i64::from(val);
            have_text_month = true;
            let dmask = m(MONTH);
            if fmask & dmask != 0 {
                return Err(DtErr::BadFormat);
            }
            fmask |= dmask;
            *tmask |= dmask;
            *f = None;
        }
    }
    for text in fields.into_iter().flatten() {
        if text.is_empty() {
            return Err(DtErr::BadFormat);
        }
        let mut dmask = 0;
        let mut fsec = 0;
        decode_number(text.len(), text, have_text_month, fmask, &mut dmask, tm, &mut fsec, is2digits)?;
        if fmask & dmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= dmask;
        *tmask |= dmask;
    }
    if fmask & !(m(DOY) | m(TZ)) != DTK_DATE_M {
        return Err(DtErr::BadFormat);
    }
    Ok(())
}

/// ValidateDate: the year as the calendar counts it (2-digit years, BC), the day of the year, and the fields' ranges.
fn validate_date(fmask: u32, isjulian: bool, is2digits: bool, bc: bool, tm: &mut Tm) -> R<()> {
    if fmask & m(YEAR) != 0 {
        if isjulian {
        } else if bc {
            if tm.year <= 0 {
                return Err(DtErr::FieldOverflow);
            }
            tm.year = -(tm.year - 1); // (1 BC is the calendar's year 0)
        } else if is2digits {
            if tm.year < 0 {
                return Err(DtErr::FieldOverflow);
            }
            if tm.year < 70 {
                tm.year += 2000;
            } else if tm.year < 100 {
                tm.year += 1900;
            }
        } else if tm.year <= 0 {
            return Err(DtErr::FieldOverflow);
        }
    }
    if fmask & m(DOY) != 0 {
        let (y, mo, d) = civil_from_days(days_from_civil(tm.year, 1, 1) + tm.yday - 1);
        (tm.year, tm.mon, tm.mday) = (y, i64::from(mo), i64::from(d));
    }
    if fmask & m(MONTH) != 0 && !(1..=12).contains(&tm.mon) {
        return Err(DtErr::MdFieldOverflow);
    }
    if fmask & m(DAYF) != 0 && !(1..=31).contains(&tm.mday) {
        return Err(DtErr::MdFieldOverflow);
    }
    if fmask & DTK_DATE_M == DTK_DATE_M && tm.mday > i64::from(days_in_month(tm.year, tm.mon as u32)) {
        return Err(DtErr::FieldOverflow);
    }
    Ok(())
}

/// time_overflows: a time of day past 24:00:00, or a field past its range.
fn time_overflows(tm: &Tm, fsec: i64) -> bool {
    if !(0..=24).contains(&tm.hour) || !(0..60).contains(&tm.min) || !(0..=60).contains(&tm.sec) {
        return true;
    }
    if !(0..=1_000_000).contains(&fsec) {
        return true;
    }
    ((tm.hour * 60 + tm.min) * 60 + tm.sec) * SECOND + fsec > DAY
}

/// The calendar date of a Julian day number.
fn j2date(jd: i64, tm: &mut Tm) {
    let (y, mo, d) = civil_from_days(jd - 2_440_588);
    (tm.year, tm.mon, tm.mday) = (y, i64::from(mo), i64::from(d));
}

/// A time of day in microseconds, into `tm`; the microseconds of its second are returned.
fn dt2time(t: i64, tm: &mut Tm) -> i64 {
    tm.hour = t / HOUR;
    tm.min = t % HOUR / MINUTE;
    tm.sec = t % MINUTE / SECOND;
    t % SECOND
}

/// The session's clock now, as local fields (GetCurrentDateTime / GetCurrentTimeUsec), and its zone's offset west.
fn current() -> (Tm, i64, i32) {
    let t = now();
    let off = offset_at(t);
    let local = t + off;
    let mut tm = Tm::default();
    let day = local.div_euclid(DAY);
    j2date(day + 2_440_588, &mut tm);
    let fsec = dt2time(local.rem_euclid(DAY), &mut tm);
    (tm, fsec, -(off / SECOND) as i32)
}

/// A wall clock reading's microseconds since 1970 (as if UTC), if it fits.
fn local_micros(tm: &Tm, fsec: i64) -> Option<i64> {
    if !(1..=12).contains(&tm.mon) || !(1..=31).contains(&tm.mday) {
        return None;
    }
    let days = i128::from(days_from_civil(tm.year, tm.mon as u32, tm.mday as u32));
    let time = i128::from(((tm.hour * 60 + tm.min) * 60 + tm.sec) * SECOND + fsec);
    i64::try_from(days * i128::from(DAY) + time).ok()
}

/// DetermineTimeZoneOffset for the session's zone: its offset west of UTC at that wall clock reading (0 when
/// the reading is out of range).
fn session_tz(tm: &Tm) -> i32 {
    match local_micros(tm, 0) {
        Some(l) => -(offset_for_local(l) / SECOND) as i32,
        None => 0,
    }
}

/// DetermineTimeZoneOffset for a named zone.
fn named_tz(tm: &Tm, rules: &super::super::tz::Rules) -> i32 {
    match local_micros(tm, 0) {
        Some(l) => {
            let local = l.div_euclid(SECOND);
            (rules.utc(local) - local) as i32
        }
        None => 0,
    }
}

// ---------------------------------------------------------------- DecodeDateTime

/// What DecodeDateTime and DecodeTimeOnly read: a date (and time), or a special value.
struct Decoded {
    dtype: i32,
    tm: Tm,
    fsec: i64,
    /// the offset west of UTC, in seconds
    tz: i32,
}

/// DecodeDateTime: a date and time (timestamp, timestamptz and date input).
fn decode_date_time(fields: &[(String, Ft)]) -> R<Decoded> {
    let mut fmask: u32 = 0;
    let mut ptype: i32 = 0;
    let mut mer = HR24;
    let mut have_text_month = false;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut named: Option<std::sync::Arc<super::super::tz::Rules>> = None;
    let mut dtype = DTK_DATE;
    let mut tm = Tm::default();
    let mut fsec: i64 = 0;
    let mut tz: i32 = 0;
    for f in fields {
        let (field, ftype) = (f.0.clone(), f.1);
        let fb = field.as_bytes();
        let mut tmask: u32 = 0;
        match ftype {
            Ft::Date => {
                if ptype == DTK_JULIAN {
                    // a Julian day with its zone stuck on: J2452271-08
                    let (jday, cp) = strtoint(fb, 0, DtErr::FieldOverflow)?;
                    if jday < 0 {
                        return Err(DtErr::FieldOverflow);
                    }
                    j2date(jday, &mut tm);
                    isjulian = true;
                    tz = decode_timezone(&field[cp..])?;
                    tmask = DTK_DATE_M | DTK_TIME_M | m(TZ);
                    ptype = 0;
                } else if ptype != 0 || fmask & (m(MONTH) | m(DAYF)) == m(MONTH) | m(DAYF) {
                    // with the date known: a zone's name (America/New_York), or a run-together time and its zone
                    if fb[0].is_ascii_digit() || ptype != 0 {
                        if ptype != 0 {
                            if ptype != DTK_TIME {
                                return Err(DtErr::BadFormat);
                            }
                            ptype = 0;
                        }
                        if fmask & DTK_TIME_M == DTK_TIME_M {
                            return Err(DtErr::BadFormat);
                        }
                        let Some(cp) = field.find('-') else { return Err(DtErr::BadFormat) };
                        tz = decode_timezone(&field[cp..])?;
                        decode_number_field(&field[..cp], fmask, &mut tmask, &mut tm, &mut fsec, &mut is2digits)?;
                        tmask |= m(TZ);
                    } else {
                        match zone_rules(&field) {
                            Some(r) => named = Some(r),
                            None => return Err(DtErr::BadTimezone(field.clone())),
                        }
                        tmask = m(TZ);
                    }
                } else {
                    decode_date(&field, fmask, &mut tmask, &mut is2digits, &mut tm)?;
                }
            }
            Ft::Time => {
                // (maybe an ISO time after a "t")
                if ptype != 0 {
                    if ptype != DTK_TIME {
                        return Err(DtErr::BadFormat);
                    }
                    ptype = 0;
                }
                fsec = decode_time(&field, &mut tm)?;
                tmask = DTK_TIME_M;
                if time_overflows(&tm, fsec) {
                    return Err(DtErr::FieldOverflow);
                }
            }
            Ft::Tz => {
                tz = decode_timezone(&field)?;
                tmask = m(TZ);
            }
            Ft::Number => {
                if ptype != 0 {
                    // a number a word before it labeled: J2452271, T040506
                    let (value, cp) = strtoint(fb, 0, DtErr::FieldOverflow)?;
                    if cp < fb.len() && fb[cp] != b'.' {
                        return Err(DtErr::BadFormat);
                    }
                    match ptype {
                        DTK_JULIAN => {
                            if value < 0 {
                                return Err(DtErr::FieldOverflow);
                            }
                            tmask = DTK_DATE_M;
                            j2date(value, &mut tm);
                            isjulian = true;
                            // a fraction of a Julian day is its time
                            if cp < fb.len() {
                                let t = parse_fraction(&field[cp..])? * DAY as f64;
                                fsec = dt2time(t as i64, &mut tm);
                                tmask |= DTK_TIME_M;
                            }
                        }
                        DTK_TIME => {
                            decode_number_field(
                                &field,
                                fmask | DTK_DATE_M,
                                &mut tmask,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            if tmask != DTK_TIME_M {
                                return Err(DtErr::BadFormat);
                            }
                        }
                        _ => return Err(DtErr::BadFormat),
                    }
                    ptype = 0;
                    dtype = DTK_DATE;
                } else {
                    let flen = field.len();
                    let dot = field.find('.');
                    if dot.is_some() && fmask & DTK_DATE_M == 0 {
                        decode_date(&field, fmask, &mut tmask, &mut is2digits, &mut tm)?;
                    } else if let Some(d) = dot
                        && d > 2
                    {
                        decode_number_field(&field, fmask, &mut tmask, &mut tm, &mut fsec, &mut is2digits)?;
                    } else if flen >= 6 && (fmask & DTK_DATE_M == 0 || fmask & DTK_TIME_M == 0) {
                        // six digits or more: YMD or HMS, unless both are known (then a year)
                        decode_number_field(&field, fmask, &mut tmask, &mut tm, &mut fsec, &mut is2digits)?;
                    } else {
                        decode_number(
                            flen,
                            &field,
                            have_text_month,
                            fmask,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    }
                }
            }
            Ft::String | Ft::Special => {
                // a dynamic abbreviation (`MSK`): its offset is the one it had at this date and time (DYNTZ)
                if let Some(r) = super::super::tz::dynamic_abbrev(&field) {
                    named = Some(r);
                    tmask = m(TZ);
                    if tmask & fmask != 0 {
                        return Err(DtErr::BadFormat);
                    }
                    fmask |= tmask;
                    continue;
                }
                // (zone abbreviations come before the keywords)
                let (ty, val) = abbrev(&field).or_else(|| lookup(DATETK, &field)).unwrap_or((UNKNOWN_FIELD, 0));
                if ty == IGNORE_DTF {
                    continue;
                }
                tmask = m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = DTK_DATE_M | DTK_TIME_M | m(TZ);
                            dtype = DTK_DATE;
                            let (cur, f, z) = current();
                            tm = Tm { yday: tm.yday, ..cur };
                            fsec = f;
                            tz = z;
                        }
                        DTK_YESTERDAY | DTK_TODAY | DTK_TOMORROW => {
                            tmask = DTK_DATE_M;
                            dtype = DTK_DATE;
                            let (cur, ..) = current();
                            let shift = match val {
                                DTK_YESTERDAY => -1,
                                DTK_TOMORROW => 1,
                                _ => 0,
                            };
                            let jd = days_from_civil(cur.year, cur.mon as u32, cur.mday as u32) + 2_440_588 + shift;
                            j2date(jd, &mut tm);
                        }
                        DTK_ZULU => {
                            tmask = DTK_TIME_M | m(TZ);
                            dtype = DTK_DATE;
                            (tm.hour, tm.min, tm.sec) = (0, 0, 0);
                            tz = 0;
                        }
                        _ => {
                            // epoch, infinity, -infinity
                            tmask = DTK_DATE_M | DTK_TIME_M | m(TZ);
                            dtype = val;
                        }
                    },
                    MONTH => {
                        // a number already read as the month was the day
                        if fmask & m(MONTH) != 0
                            && !have_text_month
                            && fmask & m(DAYF) == 0
                            && (1..=31).contains(&tm.mon)
                        {
                            tm.mday = tm.mon;
                            tmask = m(DAYF);
                        }
                        have_text_month = true;
                        tm.mon = i64::from(val);
                    }
                    DTZMOD => {
                        // `MET DST`: daylight saving time on top of a zone
                        tmask |= m(DTZ);
                        tz -= val;
                    }
                    DTZ => {
                        tmask |= m(TZ);
                        tz = -val;
                    }
                    TZ => tz = -val,
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    DOW => {}
                    UNITS => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(DtErr::BadFormat);
                        }
                        ptype = val;
                    }
                    ISOTIME => {
                        // `t`: the time follows; there must be a date before it
                        tmask = 0;
                        if fmask & DTK_DATE_M != DTK_DATE_M || ptype != 0 {
                            return Err(DtErr::BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        // a zone's name of letters alone (`UTC`, `Zulu`)
                        match zone_rules(&field) {
                            Some(r) => named = Some(r),
                            None => return Err(DtErr::BadFormat),
                        }
                        tmask = m(TZ);
                    }
                    _ => return Err(DtErr::BadFormat),
                }
            }
        }
        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }
    if ptype != 0 {
        return Err(DtErr::BadFormat);
    }
    if dtype == DTK_DATE {
        validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
        if mer != HR24 && tm.hour > 12 {
            return Err(DtErr::FieldOverflow);
        }
        if mer == AM && tm.hour == 12 {
            tm.hour = 0;
        } else if mer == PM && tm.hour != 12 {
            tm.hour += 12;
        }
        if fmask & DTK_DATE_M != DTK_DATE_M {
            return Err(DtErr::BadFormat); // (a time alone, or not even that)
        }
        if let Some(r) = &named {
            if fmask & m(DTZMOD) != 0 {
                return Err(DtErr::BadFormat);
            }
            tz = named_tz(&tm, r);
        }
        if fmask & m(TZ) == 0 {
            if fmask & m(DTZMOD) != 0 {
                return Err(DtErr::BadFormat);
            }
            tz = session_tz(&tm);
        }
    }
    Ok(Decoded { dtype, tm, fsec, tz })
}

/// DecodeTimeOnly: a time of day (time and timetz input), maybe with a date to say which offset a zone has.
fn decode_time_only(fields: &mut [(String, Ft)]) -> R<Decoded> {
    let mut fmask: u32 = 0;
    let mut ptype: i32 = 0;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut mer = HR24;
    let mut named: Option<std::sync::Arc<super::super::tz::Rules>> = None;
    let mut dtype = DTK_TIME;
    let mut tm = Tm::default();
    let mut fsec: i64 = 0;
    let mut tz: i32 = 0;
    let nf = fields.len();
    for i in 0..nf {
        let (field, ftype) = (fields[i].0.clone(), fields[i].1);
        let fb = field.as_bytes();
        let mut tmask: u32 = 0;
        match ftype {
            Ft::Date => {
                // a date, at the start and with more after it
                if i == 0 && nf >= 2 && (fields[nf - 1].1 == Ft::Date || fields[1].1 == Ft::Time) {
                    decode_date(&field, fmask, &mut tmask, &mut is2digits, &mut tm)?;
                } else if fb[0].is_ascii_digit() {
                    if fmask & DTK_TIME_M == DTK_TIME_M {
                        return Err(DtErr::BadFormat);
                    }
                    let Some(cp) = field.find('-') else { return Err(DtErr::BadFormat) };
                    tz = decode_timezone(&field[cp..])?;
                    let t = decode_number_field(
                        &field[..cp],
                        fmask | DTK_DATE_M,
                        &mut tmask,
                        &mut tm,
                        &mut fsec,
                        &mut is2digits,
                    )?;
                    fields[i].1 = if t == DTK_DATE { Ft::Date } else { Ft::Time };
                    tmask |= m(TZ);
                } else {
                    match zone_rules(&field) {
                        Some(r) => named = Some(r),
                        None => return Err(DtErr::BadTimezone(field.clone())),
                    }
                    fields[i].1 = Ft::Tz;
                    tmask = m(TZ);
                }
            }
            Ft::Time => {
                if ptype != 0 {
                    if ptype != DTK_TIME {
                        return Err(DtErr::BadFormat);
                    }
                    ptype = 0;
                }
                fsec = decode_time(&field, &mut tm)?;
                tmask = DTK_TIME_M;
            }
            Ft::Tz => {
                tz = decode_timezone(&field)?;
                tmask = m(TZ);
            }
            Ft::Number => {
                if ptype != 0 {
                    let (value, cp) = strtoint(fb, 0, DtErr::FieldOverflow)?;
                    if cp < fb.len() && fb[cp] != b'.' {
                        return Err(DtErr::BadFormat);
                    }
                    match ptype {
                        DTK_JULIAN => {
                            if value < 0 {
                                return Err(DtErr::FieldOverflow);
                            }
                            tmask = DTK_DATE_M;
                            j2date(value, &mut tm);
                            isjulian = true;
                            if cp < fb.len() {
                                let t = parse_fraction(&field[cp..])? * DAY as f64;
                                fsec = dt2time(t as i64, &mut tm);
                                tmask |= DTK_TIME_M;
                            }
                        }
                        DTK_TIME => {
                            let t = decode_number_field(
                                &field,
                                fmask | DTK_DATE_M,
                                &mut tmask,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            fields[i].1 = if t == DTK_DATE { Ft::Date } else { Ft::Time };
                            if tmask != DTK_TIME_M {
                                return Err(DtErr::BadFormat);
                            }
                        }
                        _ => return Err(DtErr::BadFormat),
                    }
                    ptype = 0;
                    dtype = DTK_DATE;
                } else {
                    let flen = field.len();
                    if let Some(dot) = field.find('.') {
                        if i == 0 && nf >= 2 && fields[nf - 1].1 == Ft::Date {
                            decode_date(&field, fmask, &mut tmask, &mut is2digits, &mut tm)?;
                        } else if dot > 2 {
                            let t = decode_number_field(
                                &field,
                                fmask | DTK_DATE_M,
                                &mut tmask,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            fields[i].1 = if t == DTK_DATE { Ft::Date } else { Ft::Time };
                        } else {
                            return Err(DtErr::BadFormat);
                        }
                    } else if flen > 4 {
                        let t = decode_number_field(
                            &field,
                            fmask | DTK_DATE_M,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        fields[i].1 = if t == DTK_DATE { Ft::Date } else { Ft::Time };
                    } else {
                        decode_number(
                            flen,
                            &field,
                            false,
                            fmask | DTK_DATE_M,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    }
                }
            }
            Ft::String | Ft::Special => {
                let (ty, val) = abbrev(&field).or_else(|| lookup(DATETK, &field)).unwrap_or((UNKNOWN_FIELD, 0));
                if ty == IGNORE_DTF {
                    continue;
                }
                tmask = m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = DTK_TIME_M;
                            dtype = DTK_TIME;
                            let (cur, f, _) = current();
                            (tm.hour, tm.min, tm.sec) = (cur.hour, cur.min, cur.sec);
                            fsec = f;
                        }
                        DTK_ZULU => {
                            tmask = DTK_TIME_M | m(TZ);
                            dtype = DTK_TIME;
                            (tm.hour, tm.min, tm.sec) = (0, 0, 0);
                        }
                        _ => return Err(DtErr::BadFormat),
                    },
                    DTZMOD => {
                        tmask |= m(DTZ);
                        tz -= val;
                    }
                    DTZ => {
                        tmask |= m(TZ);
                        tz = -val;
                        fields[i].1 = Ft::Tz;
                    }
                    TZ => {
                        tz = -val;
                        fields[i].1 = Ft::Tz;
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    UNITS | ISOTIME => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(DtErr::BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        match zone_rules(&field) {
                            Some(r) => named = Some(r),
                            None => return Err(DtErr::BadFormat),
                        }
                        tmask = m(TZ);
                    }
                    _ => return Err(DtErr::BadFormat),
                }
            }
        }
        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }
    if ptype != 0 {
        return Err(DtErr::BadFormat);
    }
    validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
    if mer != HR24 && tm.hour > 12 {
        return Err(DtErr::FieldOverflow);
    }
    if mer == AM && tm.hour == 12 {
        tm.hour = 0;
    } else if mer == PM && tm.hour != 12 {
        tm.hour += 12;
    }
    if time_overflows(&tm, fsec) {
        return Err(DtErr::FieldOverflow);
    }
    if fmask & DTK_TIME_M != DTK_TIME_M {
        return Err(DtErr::BadFormat);
    }
    if let Some(r) = &named {
        if fmask & m(DTZMOD) != 0 {
            return Err(DtErr::BadFormat);
        }
        // (a zone with one offset needs no date; one with daylight saving time does)
        match r.fixed_offset() {
            Some(off) => tz = -off,
            None => {
                if fmask & DTK_DATE_M != DTK_DATE_M {
                    return Err(DtErr::BadFormat);
                }
                tz = named_tz(&tm, r);
            }
        }
    }
    if fmask & m(TZ) == 0 {
        if fmask & m(DTZMOD) != 0 {
            return Err(DtErr::BadFormat);
        }
        let mut t = if fmask & DTK_DATE_M == 0 {
            current().0
        } else {
            if fmask & DTK_DATE_M != DTK_DATE_M {
                return Err(DtErr::BadFormat);
            }
            tm
        };
        (t.hour, t.min, t.sec) = (tm.hour, tm.min, tm.sec);
        tz = session_tz(&t);
    }
    Ok(Decoded { dtype, tm, fsec, tz })
}

/// IS_VALID_JULIAN: a date the Julian day routines take (4714-11-24 BC to 5874898-06-03).
fn valid_julian(y: i64, mo: i64) -> bool {
    (y > -4713 || (y == -4713 && mo >= 11)) && (y < 5_874_898 || (y == 5_874_898 && mo < 6))
}

/// tm2timestamp: the fields as microseconds since 1970, in UTC when `tz` (west, seconds) is given.
fn tm2timestamp(tm: &Tm, fsec: i64, tz: Option<i32>) -> Option<i64> {
    if !valid_julian(tm.year, tm.mon) {
        return None;
    }
    let local = i128::from(local_micros(tm, fsec)?);
    let t = local + tz.map_or(0, |z| i128::from(z) * i128::from(SECOND));
    (i128::from(MIN_TIMESTAMP)..i128::from(TS_INF)).contains(&t).then_some(t as i64)
}

// ---------------------------------------------------------------- the readers

/// The canonical ISO 8601 form, `2024-03-10`, `2024-03-10 14:05:09.25`, `2024-03-10T14:05:09.25+05:30` (what stored
/// values are, read back on every row), read straight off its bytes: (days since 1970, µs of the day, the offset east
/// in seconds if one is written). None for anything else, which the full reader takes (as it does a year 0, hour 24,
/// second 60, or more than six digits of a second, whose answers that reader has).
fn iso_fast(s: &str) -> Option<(i64, i64, Option<i64>)> {
    let b = s.as_bytes();
    let digits = |at: usize, n: usize| -> Option<i64> {
        let part = b.get(at..at + n)?;
        part.iter().all(u8::is_ascii_digit).then(|| part.iter().fold(0i64, |v, d| v * 10 + i64::from(d - b'0')))
    };
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let (y, mo, d) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    if y == 0 || !(1..=12).contains(&mo) || d == 0 || d > i64::from(days_in_month(y, mo as u32)) {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    if b.len() == 10 {
        return Some((days, 0, None));
    }
    if !matches!(b[10], b' ' | b'T') || b.get(13) != Some(&b':') || b.get(16) != Some(&b':') {
        return None;
    }
    let (h, mi, se) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    if h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let mut at = 19;
    let mut frac = 0;
    if b.get(at) == Some(&b'.') {
        let n = b[at + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 || n > 6 {
            return None;
        }
        frac = digits(at + 1, n)? * 10i64.pow(6 - n as u32);
        at += 1 + n;
    }
    let time = ((h * 60 + mi) * 60 + se) * SECOND + frac;
    let zone = match b.get(at) {
        None => None,
        Some(b'Z') if at + 1 == b.len() => Some(0),
        Some(c @ (b'+' | b'-')) => {
            let zh = digits(at + 1, 2)?;
            let zm = match b.len() - at {
                3 => 0,
                6 if b[at + 3] == b':' => digits(at + 4, 2)?,
                _ => return None,
            };
            if zh > 15 || zm > 59 {
                return None;
            }
            let east = (zh * 60 + zm) * 60;
            Some(if *c == b'-' { -east } else { east })
        }
        Some(_) => return None,
    };
    Some((days, time, zone))
}

/// timestamp_in and timestamptz_in: text as microseconds since 1970 (UTC for `tz`).
pub(crate) fn timestamp(s: &str, tz: bool) -> Result<i64, Bad> {
    if let Some((days, time, zone)) = iso_fast(s) {
        let local = days * DAY + time;
        return Ok(match (tz, zone) {
            (false, _) => local,
            (true, Some(east)) => local - east * SECOND,
            (true, None) => local - offset_for_local(local),
        });
    }
    let f = parse_fields(s).map_err(DtErr::bad)?;
    let d = decode_date_time(&f).map_err(DtErr::bad)?;
    match d.dtype {
        DTK_DATE => tm2timestamp(&d.tm, d.fsec, tz.then_some(d.tz)).ok_or(Bad::Range),
        DTK_EPOCH => Ok(0),
        DTK_LATE => Ok(super::TS_INF),
        DTK_EARLY => Ok(super::TS_NEG_INF),
        _ => Err(Bad::Syntax),
    }
}

/// date_in: text as days since 1970.
pub(crate) fn date(s: &str) -> Result<i32, Bad> {
    if let Some((days, ..)) = iso_fast(s) {
        return Ok(days as i32); // (a year of four digits: always a date)
    }
    let f = parse_fields(s).map_err(DtErr::bad)?;
    let d = decode_date_time(&f).map_err(DtErr::bad)?;
    let tm = match d.dtype {
        DTK_DATE => d.tm,
        DTK_EPOCH => Tm { year: 1970, mon: 1, mday: 1, ..Tm::default() },
        DTK_LATE => return Ok(super::DATE_INF),
        DTK_EARLY => return Ok(super::DATE_NEG_INF),
        _ => return Err(Bad::Syntax),
    };
    if !valid_julian(tm.year, tm.mon) {
        return Err(Bad::Range);
    }
    let days = days_from_civil(tm.year, tm.mon as u32, tm.mday as u32);
    // (IS_VALID_DATE: Julian day 0, 4714-11-24 BC, to 5874898-01-01)
    if !(0..2_147_483_494).contains(&(days + 2_440_588)) {
        return Err(Bad::Range);
    }
    i32::try_from(days).map_err(|_| Bad::Range)
}

/// time_in and timetz_in: text as (microseconds since midnight, offset east of UTC in seconds).
pub(crate) fn time(s: &str) -> Result<(i64, i32), Bad> {
    // (`HH:MM:SS[.ffffff]`, as stored times are written, straight off its bytes; its zone is the session's)
    let b = s.as_bytes();
    if b.len() >= 8 && b.len() <= 15 && b[2] == b':' && b[5] == b':' && (b.len() == 8 || b[8] == b'.') {
        let digits = |r: std::ops::Range<usize>| {
            b[r.clone()]
                .iter()
                .all(u8::is_ascii_digit)
                .then(|| b[r].iter().fold(0i64, |v, d| v * 10 + i64::from(d - b'0')))
        };
        let frac = match b.len() {
            8 => Some(0),
            9 => None,
            n => digits(9..n),
        };
        if let (Some(h), Some(mi), Some(se), Some(frac)) = (digits(0..2), digits(3..5), digits(6..8), frac)
            && h < 24
            && mi < 60
            && se < 60
        {
            let us = ((h * 60 + mi) * 60 + se) * SECOND + frac * 10i64.pow(6 - (b.len().max(9) - 9) as u32);
            let (now_tm, ..) = current();
            let t = Tm { hour: h, min: mi, sec: se, ..now_tm };
            return Ok((us, -session_tz(&t)));
        }
    }
    let mut f = parse_fields(s).map_err(DtErr::bad)?;
    let d = decode_time_only(&mut f).map_err(DtErr::bad)?;
    let us = ((d.tm.hour * 60 + d.tm.min) * 60 + d.tm.sec) * SECOND + d.fsec;
    Ok((us, -d.tz))
}

/// Why make_timestamptz's zone isn't one (parse_sane_timezone): it starts with a digit, or is a number out of range.
pub(crate) enum SaneZoneErr {
    NoSign,
    OutOfRange,
}

/// parse_sane_timezone's first step: `+2`, `-08:30`, `+1000` as seconds east of UTC (ISO's sign); None when the
/// text isn't a numeric zone, which is then a name or an abbreviation.
pub(crate) fn sane_numeric_zone(s: &str) -> Result<Option<i32>, SaneZoneErr> {
    if s.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return Err(SaneZoneErr::NoSign);
    }
    match decode_timezone(s) {
        Ok(west) => Ok(Some(-west)),
        Err(DtErr::TzDispOverflow) => Err(SaneZoneErr::OutOfRange),
        Err(_) => Ok(None),
    }
}

// ---------------------------------------------------------------- intervals

/// pg_itm_in: an interval's fields as they are read (years and months apart).
#[derive(Default)]
struct Itm {
    usec: i64,
    mday: i32,
    mon: i32,
    year: i32,
}

fn adjust_fract_microseconds(frac: f64, scale: i64, itm: &mut Itm) -> bool {
    if frac == 0.0 {
        return true;
    }
    let frac = frac * scale as f64;
    let mut usec = frac as i64;
    let rest = frac - usec as f64;
    if rest > 0.5 {
        usec += 1;
    } else if rest < -0.5 {
        usec -= 1;
    }
    match itm.usec.checked_add(usec) {
        Some(u) => {
            itm.usec = u;
            true
        }
        None => false,
    }
}

fn adjust_fract_days(frac: f64, scale: i32, itm: &mut Itm) -> bool {
    if frac == 0.0 {
        return true;
    }
    let frac = frac * f64::from(scale);
    let extra = frac as i32;
    let Some(d) = itm.mday.checked_add(extra) else { return false };
    itm.mday = d;
    adjust_fract_microseconds(frac - f64::from(extra), DAY, itm)
}

fn adjust_fract_years(frac: f64, scale: i32, itm: &mut Itm) -> bool {
    let extra = (frac * f64::from(scale) * 12.0).round_ties_even() as i32;
    match itm.mon.checked_add(extra) {
        Some(mo) => {
            itm.mon = mo;
            true
        }
        None => false,
    }
}

fn adjust_microseconds(val: i64, fval: f64, scale: i64, itm: &mut Itm) -> bool {
    let Some(u) = val.checked_mul(scale).and_then(|p| itm.usec.checked_add(p)) else { return false };
    itm.usec = u;
    adjust_fract_microseconds(fval, scale, itm)
}

fn adjust_days(val: i64, scale: i32, itm: &mut Itm) -> bool {
    let Ok(v) = i32::try_from(val) else { return false };
    match v.checked_mul(scale).and_then(|d| itm.mday.checked_add(d)) {
        Some(d) => {
            itm.mday = d;
            true
        }
        None => false,
    }
}

fn adjust_months(val: i64, itm: &mut Itm) -> bool {
    let Ok(v) = i32::try_from(val) else { return false };
    match itm.mon.checked_add(v) {
        Some(mo) => {
            itm.mon = mo;
            true
        }
        None => false,
    }
}

fn adjust_years(val: i64, scale: i32, itm: &mut Itm) -> bool {
    let Ok(v) = i32::try_from(val) else { return false };
    match v.checked_mul(scale).and_then(|y| itm.year.checked_add(y)) {
        Some(y) => {
            itm.year = y;
            true
        }
        None => false,
    }
}

/// DecodeTimeForInterval: `hh:mm:ss` of an interval, as its microseconds (replacing any before).
fn decode_time_for_interval(s: &str, range: u32, itm: &mut Itm) -> R<()> {
    let (h, mi, se, fsec) = decode_time_common(s, range)?;
    itm.usec = fsec;
    let add = |u: i64, v: i64, scale: i64| v.checked_mul(scale).and_then(|p| u.checked_add(p));
    itm.usec = add(itm.usec, h, HOUR)
        .and_then(|u| add(u, mi, MINUTE))
        .and_then(|u| add(u, se, SECOND))
        .ok_or(DtErr::FieldOverflow)?;
    Ok(())
}

/// DecodeInterval: `1 day 02:03`, `@ 2 hours ago`, `1-2`, `-1 +02:03`, read from the last field to the first.
fn decode_interval(fields: &[(String, Ft)], range: u32) -> R<Itm> {
    let mut itm = Itm::default();
    let mut fmask: u32 = 0;
    let mut is_before = false;
    // (the SQL standard's '-1 1:00:00' is negative throughout, when no other field has a sign)
    let mut force_negative = false;
    if super::INTERVAL_STYLE.get() == IntervalStyle::SqlStandard && fields.first().is_some_and(|f| f.0.starts_with('-'))
    {
        force_negative = !fields[1..].iter().any(|f| f.0.starts_with(['-', '+']));
    }
    let mut ty: i32 = IGNORE_DTF as i32;
    for i in (0..fields.len()).rev() {
        let (field, ftype) = (&fields[i].0, fields[i].1);
        let fb = field.as_bytes();
        let mut tmask: u32 = 0;
        let mut number = matches!(ftype, Ft::Date | Ft::Number);
        match ftype {
            Ft::Time => {
                decode_time_for_interval(field, range, &mut itm)?;
                tmask = DTK_TIME_M;
                if force_negative && itm.usec > 0 {
                    itm.usec = -itm.usec;
                }
                ty = DTK_DAY;
            }
            Ft::Tz => {
                // a signed hh:mm[:ss]; else a signed number, read below
                let mut t = Itm { usec: itm.usec, ..Itm::default() };
                if field[1..].contains(':') && decode_time_for_interval(&field[1..], range, &mut t).is_ok() {
                    itm.usec = t.usec;
                    tmask = DTK_TIME_M;
                    if fb[0] == b'-' {
                        if itm.usec == i64::MIN {
                            return Err(DtErr::FieldOverflow);
                        }
                        itm.usec = -itm.usec;
                    }
                    if force_negative && itm.usec > 0 {
                        itm.usec = -itm.usec;
                    }
                    ty = DTK_DAY;
                } else {
                    number = true;
                }
            }
            Ft::String | Ft::Special => {
                let (t, uval) = lookup(DELTATK, field).unwrap_or((UNKNOWN_FIELD, 0));
                if t == IGNORE_DTF {
                    continue;
                }
                match t {
                    UNITS => ty = uval,
                    AGO => {
                        is_before = true;
                        ty = uval;
                    }
                    RESERV => {
                        tmask = DTK_DATE_M | DTK_TIME_M;
                    }
                    _ => return Err(DtErr::BadFormat),
                }
            }
            _ => {}
        }
        if number {
            if ty == IGNORE_DTF as i32 {
                // the typmod says what the last field is
                ty = match range {
                    IM_YEAR => DTK_YEAR,
                    x if x == IM_MONTH || x == IM_YEAR | IM_MONTH => DTK_MONTH,
                    IM_DAY => DTK_DAY,
                    x if x == IM_HOUR || x == IM_DAY | IM_HOUR => DTK_HOUR,
                    x if x == IM_MINUTE || x == IM_HOUR | IM_MINUTE || x == IM_DAY | IM_HOUR | IM_MINUTE => DTK_MINUTE,
                    _ => DTK_SECOND,
                };
            }
            let (mut val, mut cp) = strtoi64(fb, 0)?;
            let mut fval: f64;
            if fb.get(cp) == Some(&b'-') {
                // the SQL standard's years-months
                let (v2, c) = strtol(fb, cp + 1);
                if !(0..12).contains(&v2) {
                    return Err(DtErr::FieldOverflow);
                }
                cp = c;
                if cp != fb.len() {
                    return Err(DtErr::BadFormat);
                }
                ty = DTK_MONTH;
                let v2 = if fb[0] == b'-' { -(v2 as i64) } else { v2 as i64 };
                val = val.checked_mul(12).and_then(|v| v.checked_add(v2)).ok_or(DtErr::FieldOverflow)?;
                fval = 0.0;
            } else if fb.get(cp) == Some(&b'.') {
                fval = parse_fraction(&field[cp..])?;
                if fb[0] == b'-' {
                    fval = -fval;
                }
            } else if cp == fb.len() {
                fval = 0.0;
            } else {
                return Err(DtErr::BadFormat);
            }
            if force_negative {
                if val > 0 {
                    val = -val;
                }
                if fval > 0.0 {
                    fval = -fval;
                }
            }
            let ok = match ty {
                DTK_MICROSEC => {
                    tmask = m(MICROSECOND);
                    adjust_microseconds(val, fval, 1, &mut itm)
                }
                DTK_MILLISEC => {
                    tmask = m(MILLISECOND);
                    adjust_microseconds(val, fval, 1000, &mut itm)
                }
                DTK_SECOND => {
                    // (a fraction of a second counts as its milliseconds and microseconds too)
                    tmask = if fval == 0.0 { m(SECONDF) } else { DTK_ALL_SECS_M };
                    adjust_microseconds(val, fval, SECOND, &mut itm)
                }
                DTK_MINUTE => {
                    tmask = m(MINUTEF);
                    adjust_microseconds(val, fval, MINUTE, &mut itm)
                }
                DTK_HOUR => {
                    tmask = m(HOURF);
                    ty = DTK_DAY; // (for the field before it)
                    adjust_microseconds(val, fval, HOUR, &mut itm)
                }
                DTK_DAY => {
                    tmask = m(DAYF);
                    adjust_days(val, 1, &mut itm) && adjust_fract_microseconds(fval, DAY, &mut itm)
                }
                DTK_WEEK => {
                    tmask = m(WEEK);
                    adjust_days(val, 7, &mut itm) && adjust_fract_days(fval, 7, &mut itm)
                }
                DTK_MONTH => {
                    tmask = m(MONTH);
                    adjust_months(val, &mut itm) && adjust_fract_days(fval, 30, &mut itm)
                }
                DTK_YEAR => {
                    tmask = m(YEAR);
                    adjust_years(val, 1, &mut itm) && adjust_fract_years(fval, 1, &mut itm)
                }
                DTK_DECADE => {
                    tmask = m(DECADE);
                    adjust_years(val, 10, &mut itm) && adjust_fract_years(fval, 10, &mut itm)
                }
                DTK_CENTURY => {
                    tmask = m(CENTURY);
                    adjust_years(val, 100, &mut itm) && adjust_fract_years(fval, 100, &mut itm)
                }
                DTK_MILLENNIUM => {
                    tmask = m(MILLENNIUM);
                    adjust_years(val, 1000, &mut itm) && adjust_fract_years(fval, 1000, &mut itm)
                }
                _ => return Err(DtErr::BadFormat),
            };
            if !ok {
                return Err(DtErr::FieldOverflow);
            }
        }
        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }
    if fmask == 0 {
        return Err(DtErr::BadFormat);
    }
    if is_before {
        if itm.usec == i64::MIN || itm.mday == i32::MIN || itm.mon == i32::MIN || itm.year == i32::MIN {
            return Err(DtErr::FieldOverflow);
        }
        itm = Itm { usec: -itm.usec, mday: -itm.mday, mon: -itm.mon, year: -itm.year };
    }
    Ok(itm)
}

/// ParseISO8601Number: a number of an ISO 8601 interval, split into its whole and fractional parts.
fn iso_number(b: &[u8], at: usize) -> R<(i64, f64, usize)> {
    if !b.get(at).is_some_and(|c| c.is_ascii_digit() || *c == b'-' || *c == b'.') {
        return Err(DtErr::BadFormat);
    }
    let (v, end) = strtod(b, at);
    if end == at {
        return Err(DtErr::BadFormat);
    }
    if v.is_nan() || !(-1.0e15..=1.0e15).contains(&v) {
        return Err(DtErr::FieldOverflow);
    }
    let ipart = if v >= 0.0 { v.floor() as i64 } else { -((-v).floor() as i64) };
    Ok((ipart, v - ipart as f64, end))
}

/// The digits of an ISO 8601 number's whole part.
fn iso_width(b: &[u8], at: usize) -> usize {
    let at = if b.get(at) == Some(&b'-') { at + 1 } else { at };
    b[at.min(b.len())..].iter().take_while(|c| c.is_ascii_digit()).count()
}

/// DecodeISO8601Interval: `P1Y2M3DT4H5M6S`, `P0001-02-03T04:05:06`, `P00010203T040506`.
fn decode_iso8601_interval(s: &str) -> R<Itm> {
    let mut itm = Itm::default();
    let b = s.as_bytes();
    if b.len() < 2 || b[0] != b'P' {
        return Err(DtErr::BadFormat);
    }
    let ovf = |ok: bool| if ok { Ok(()) } else { Err(DtErr::FieldOverflow) };
    let (mut datepart, mut havefield) = (true, false);
    let mut i = 1;
    while i < b.len() {
        if b[i] == b'T' {
            datepart = false;
            havefield = false;
            i += 1;
            continue;
        }
        let start = i;
        let (val, fval, end) = iso_number(b, i)?;
        i = end;
        let unit = b.get(i).copied().unwrap_or(0);
        i += 1;
        if datepart {
            match unit {
                b'Y' => ovf(adjust_years(val, 1, &mut itm) && adjust_fract_years(fval, 1, &mut itm))?,
                b'M' => ovf(adjust_months(val, &mut itm) && adjust_fract_days(fval, 30, &mut itm))?,
                b'W' => ovf(adjust_days(val, 7, &mut itm) && adjust_fract_days(fval, 7, &mut itm))?,
                b'D' => ovf(adjust_days(val, 1, &mut itm) && adjust_fract_microseconds(fval, DAY, &mut itm))?,
                b'T' | 0 | b'-' => {
                    if unit != b'-' && iso_width(b, start) == 8 && !havefield {
                        // the basic alternative format: YYYYMMDD
                        ovf(adjust_years(val / 10000, 1, &mut itm)
                            && adjust_months((val / 100) % 100, &mut itm)
                            && adjust_days(val % 100, 1, &mut itm)
                            && adjust_fract_microseconds(fval, DAY, &mut itm))?;
                        if unit == 0 {
                            return Ok(itm);
                        }
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    // the extended alternative format: YYYY-MM-DD
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    ovf(adjust_years(val, 1, &mut itm) && adjust_fract_years(fval, 1, &mut itm))?;
                    if unit == 0 {
                        return Ok(itm);
                    }
                    if unit == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    let (val, fval, end) = iso_number(b, i)?;
                    i = end;
                    ovf(adjust_months(val, &mut itm) && adjust_fract_days(fval, 30, &mut itm))?;
                    match b.get(i) {
                        None => return Ok(itm),
                        Some(b'T') => {
                            datepart = false;
                            havefield = false;
                            continue;
                        }
                        Some(b'-') => i += 1,
                        Some(_) => return Err(DtErr::BadFormat),
                    }
                    let (val, fval, end) = iso_number(b, i)?;
                    i = end;
                    ovf(adjust_days(val, 1, &mut itm) && adjust_fract_microseconds(fval, DAY, &mut itm))?;
                    match b.get(i) {
                        None => return Ok(itm),
                        Some(b'T') => {
                            datepart = false;
                            havefield = false;
                            continue;
                        }
                        Some(_) => return Err(DtErr::BadFormat),
                    }
                }
                _ => return Err(DtErr::BadFormat),
            }
        } else {
            match unit {
                b'H' => ovf(adjust_microseconds(val, fval, HOUR, &mut itm))?,
                b'M' => ovf(adjust_microseconds(val, fval, MINUTE, &mut itm))?,
                b'S' => ovf(adjust_microseconds(val, fval, SECOND, &mut itm))?,
                0 | b':' => {
                    if unit == 0 && iso_width(b, start) == 6 && !havefield {
                        // hhmmss
                        ovf(adjust_microseconds(val / 10000, 0.0, HOUR, &mut itm)
                            && adjust_microseconds((val / 100) % 100, 0.0, MINUTE, &mut itm)
                            && adjust_microseconds(val % 100, 0.0, SECOND, &mut itm)
                            && adjust_fract_microseconds(fval, 1, &mut itm))?;
                        return Ok(itm);
                    }
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    ovf(adjust_microseconds(val, fval, HOUR, &mut itm))?;
                    if unit == 0 {
                        return Ok(itm);
                    }
                    let (val, fval, end) = iso_number(b, i)?;
                    i = end;
                    ovf(adjust_microseconds(val, fval, MINUTE, &mut itm))?;
                    match b.get(i) {
                        None => return Ok(itm),
                        Some(b':') => i += 1,
                        Some(_) => return Err(DtErr::BadFormat),
                    }
                    let (val, fval, end) = iso_number(b, i)?;
                    i = end;
                    ovf(adjust_microseconds(val, fval, SECOND, &mut itm))?;
                    if i == b.len() {
                        return Ok(itm);
                    }
                    return Err(DtErr::BadFormat);
                }
                _ => return Err(DtErr::BadFormat),
            }
        }
        havefield = true;
    }
    Ok(itm)
}

/// Why an interval's text isn't one: as `Bad`, with "interval out of range" (the months past an int) apart from
/// a field out of range.
pub(crate) enum IntervalBad {
    Syntax,
    Field,
    Range,
}

/// interval_in: text as an interval, read by the typmod's fields (`range`, INTERVAL_FULL_RANGE for none) and
/// rounded to `precision` digits of seconds.
pub(crate) fn interval(s: &str, range: u32, precision: Option<u32>) -> Result<Interval, IntervalBad> {
    let parsed = parse_fields(s).and_then(|f| decode_interval(&f, range));
    let itm = match parsed {
        Err(DtErr::BadFormat) => decode_iso8601_interval(s),
        r => r,
    };
    let itm = match itm {
        Ok(i) => i,
        Err(DtErr::FieldOverflow) => return Err(IntervalBad::Field),
        Err(_) => return Err(IntervalBad::Syntax),
    };
    let months = i64::from(itm.year) * 12 + i64::from(itm.mon);
    let Ok(months) = i32::try_from(months) else { return Err(IntervalBad::Range) };
    let mut i = Interval { months, days: itm.mday, micros: itm.usec };
    adjust_for_typmod(&mut i, range, precision);
    Ok(i)
}

/// AdjustIntervalForTypmod: the fields after the last the typmod names zeroed, the seconds rounded to `precision`.
pub(crate) fn adjust_for_typmod(i: &mut Interval, range: u32, precision: Option<u32>) {
    if range == IM_YEAR {
        i.months = i.months / 12 * 12;
        (i.days, i.micros) = (0, 0);
    } else if range == IM_MONTH || range == IM_YEAR | IM_MONTH {
        (i.days, i.micros) = (0, 0);
    } else if range == IM_DAY {
        i.micros = 0;
    } else if range == IM_HOUR || range == IM_DAY | IM_HOUR {
        i.micros = i.micros / HOUR * HOUR;
    } else if range == IM_MINUTE || range == IM_DAY | IM_HOUR | IM_MINUTE || range == IM_HOUR | IM_MINUTE {
        i.micros = i.micros / MINUTE * MINUTE;
    }
    if let Some(p) = precision.filter(|p| *p < 6) {
        let scale = 10i64.pow(6 - p);
        let off = scale / 2;
        i.micros = if i.micros >= 0 {
            (i.micros.saturating_add(off)) / scale * scale
        } else {
            -((off.saturating_sub(i.micros)) / scale * scale)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(s: &str) -> Vec<(String, Ft)> {
        parse_fields(s).unwrap()
    }

    #[test]
    fn cuts_fields_as_postgres() {
        assert_eq!(
            fields("J2452271+08"),
            vec![("j".into(), Ft::String), ("2452271".into(), Ft::Number), ("+08".into(), Ft::Tz)]
        );
        assert_eq!(
            fields("2001-12-27 04:05:06.789-08"),
            vec![("2001-12-27".into(), Ft::Date), ("04:05:06.789".into(), Ft::Time), ("-08".into(), Ft::Tz)]
        );
        assert_eq!(fields("America/New_York")[0].1, Ft::Date);
    }

    #[test]
    fn reads_timestamps() {
        super::super::set_zone(None);
        let t = |s: &str| timestamp(s, true);
        // 2001-12-27 04:05:06 -08 is 12:05:06 UTC
        let want = days_from_civil(2001, 12, 27) * DAY + 12 * HOUR + 5 * MINUTE + 6 * SECOND;
        assert_eq!(t("20011227 040506-08"), Ok(want));
        assert_eq!(t("20011227T040506-08"), Ok(want));
        assert_eq!(t("2001-12-27 04:05:06-08"), Ok(want));
        assert_eq!(t("J2452271 04:05:06-08"), Ok(want));
        assert_eq!(t("Dec 27 04:05:06 2001 PST"), Ok(want));
        assert_eq!(t("2001-12-27 allballs"), Ok(days_from_civil(2001, 12, 27) * DAY));
        assert_eq!(t("1995-08-06 epoch"), Err(Bad::Syntax));
        assert_eq!(t("Feb 16 17:32:01 -0097"), Err(Bad::Zone));
        assert_eq!(t("19970710 173201 America/Does_not_exist"), Err(Bad::NoZone("america/does_not_exist".into())));
    }

    #[test]
    fn reads_times_and_intervals() {
        super::super::set_zone(None);
        assert_eq!(time("040506.789+08"), Ok((4 * HOUR + 5 * MINUTE + 6 * SECOND + 789_000, 8 * 3600)));
        assert_eq!(time("T0405").map(|t| t.0), Ok(4 * HOUR + 5 * MINUTE));
        let iv = |s: &str| interval(s, INTERVAL_FULL_RANGE, None).ok();
        assert_eq!(
            iv("1 day 02:03:04"),
            Some(Interval { months: 0, days: 1, micros: 2 * HOUR + 3 * MINUTE + 4 * SECOND })
        );
        assert_eq!(iv("1 month - 1 second").map(|i| (i.months, i.micros)), Some((1, -SECOND)));
        assert!(iv("1 day 1 day").is_none());
        assert_eq!(iv("P1Y2M3DT4H5M6S").map(|i| (i.months, i.days)), Some((14, 3)));
        assert_eq!(
            interval("1 2:03", IM_DAY | IM_HOUR | IM_MINUTE, None).ok(),
            Some(Interval { months: 0, days: 1, micros: 2 * HOUR + 3 * MINUTE })
        );
    }
}
