//! `to_char` of a timestamp, timestamptz or interval, as formatting.c's DCH_to_char writes it: the template is cut
//! into keywords (`YYYY`, `Month`, `HH24`, `IW`, `RM`, `FF3`, `TZ`, ...) with their prefixes (`FM` no padding, `TM`
//! the names unpadded) and suffixes (`TH`/`th` ordinals), `"quoted text"` (a backslash quotes the next character)
//! and other characters, which are copied.

use super::{DAY, HOUR, Interval, MINUTE, SECOND, civil_from_days, days_from_civil};

/// The template keywords (DCH_poz). Lower-case spellings that mean the same share one.
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
#[derive(Clone, Copy, PartialEq, Debug)]
enum K {
    AD_,
    AM_,
    AD,
    AM,
    BC_,
    BC,
    CC,
    DAY,
    DDD,
    DD,
    DY,
    Day,
    Dy,
    D,
    FF(u8),
    FX,
    HH24,
    HH12,
    HH,
    IDDD,
    ID,
    IW,
    IYYY,
    IYY,
    IY,
    I,
    J,
    MI,
    MM,
    MONTH,
    MON,
    MS,
    Month,
    Mon,
    OF,
    PM_,
    PM,
    Q,
    RM,
    SSSS,
    SS,
    TZH,
    TZM,
    TZ,
    US,
    WW,
    W,
    Y_YYY,
    YYYY,
    YYY,
    YY,
    Y,
    ad_,
    am_,
    ad,
    am,
    bc_,
    bc,
    day,
    dy,
    month,
    mon,
    pm_,
    pm,
    rm,
    tz,
}

/// DCH_keywords, in Postgres's order: a keyword is the first one the template starts with.
const KEYWORDS: &[(&str, K)] = &[
    ("A.D.", K::AD_),
    ("A.M.", K::AM_),
    ("AD", K::AD),
    ("AM", K::AM),
    ("B.C.", K::BC_),
    ("BC", K::BC),
    ("CC", K::CC),
    ("DAY", K::DAY),
    ("DDD", K::DDD),
    ("DD", K::DD),
    ("DY", K::DY),
    ("Day", K::Day),
    ("Dy", K::Dy),
    ("D", K::D),
    ("FF1", K::FF(1)),
    ("FF2", K::FF(2)),
    ("FF3", K::FF(3)),
    ("FF4", K::FF(4)),
    ("FF5", K::FF(5)),
    ("FF6", K::FF(6)),
    ("FX", K::FX),
    ("HH24", K::HH24),
    ("HH12", K::HH12),
    ("HH", K::HH),
    ("IDDD", K::IDDD),
    ("ID", K::ID),
    ("IW", K::IW),
    ("IYYY", K::IYYY),
    ("IYY", K::IYY),
    ("IY", K::IY),
    ("I", K::I),
    ("J", K::J),
    ("MI", K::MI),
    ("MM", K::MM),
    ("MONTH", K::MONTH),
    ("MON", K::MON),
    ("MS", K::MS),
    ("Month", K::Month),
    ("Mon", K::Mon),
    ("OF", K::OF),
    ("P.M.", K::PM_),
    ("PM", K::PM),
    ("Q", K::Q),
    ("RM", K::RM),
    ("SSSSS", K::SSSS),
    ("SSSS", K::SSSS),
    ("SS", K::SS),
    ("TZH", K::TZH),
    ("TZM", K::TZM),
    ("TZ", K::TZ),
    ("US", K::US),
    ("WW", K::WW),
    ("W", K::W),
    ("Y,YYY", K::Y_YYY),
    ("YYYY", K::YYYY),
    ("YYY", K::YYY),
    ("YY", K::YY),
    ("Y", K::Y),
    ("a.d.", K::ad_),
    ("a.m.", K::am_),
    ("ad", K::ad),
    ("am", K::am),
    ("b.c.", K::bc_),
    ("bc", K::bc),
    ("cc", K::CC),
    ("day", K::day),
    ("ddd", K::DDD),
    ("dd", K::DD),
    ("dy", K::dy),
    ("d", K::D),
    ("ff1", K::FF(1)),
    ("ff2", K::FF(2)),
    ("ff3", K::FF(3)),
    ("ff4", K::FF(4)),
    ("ff5", K::FF(5)),
    ("ff6", K::FF(6)),
    ("fx", K::FX),
    ("hh24", K::HH24),
    ("hh12", K::HH12),
    ("hh", K::HH),
    ("iddd", K::IDDD),
    ("id", K::ID),
    ("iw", K::IW),
    ("iyyy", K::IYYY),
    ("iyy", K::IYY),
    ("iy", K::IY),
    ("i", K::I),
    ("j", K::J),
    ("mi", K::MI),
    ("mm", K::MM),
    ("month", K::month),
    ("mon", K::mon),
    ("ms", K::MS),
    ("of", K::OF),
    ("p.m.", K::pm_),
    ("pm", K::pm),
    ("q", K::Q),
    ("rm", K::rm),
    ("sssss", K::SSSS),
    ("ssss", K::SSSS),
    ("ss", K::SS),
    ("tzh", K::TZH),
    ("tzm", K::TZM),
    ("tz", K::tz),
    ("us", K::US),
    ("ww", K::WW),
    ("w", K::W),
    ("y,yyy", K::Y_YYY),
    ("yyyy", K::YYYY),
    ("yyy", K::YYY),
    ("yy", K::YY),
    ("y", K::Y),
];

const S_FM: u8 = 1;
const S_TM: u8 = 2;
const S_TH: u8 = 4;
const S_LTH: u8 = 8;
const S_SP: u8 = 16;

/// A piece of the template: a keyword with its prefix and suffix, or text to copy.
enum Node {
    Action(K, u8),
    Text(String),
}

/// parse_format for the date and time keywords.
fn parse_format(fmt: &str) -> Vec<Node> {
    let mut out = vec![];
    let mut s = fmt;
    while !s.is_empty() {
        let mut suffix = 0;
        // a prefix (taken even when no keyword follows it)
        for (p, id) in [("FM", S_FM), ("fm", S_FM), ("TM", S_TM), ("tm", S_TM)] {
            if let Some(r) = s.strip_prefix(p) {
                suffix |= id;
                s = r;
                break;
            }
        }
        if s.is_empty() {
            break;
        }
        if let Some((name, k)) = KEYWORDS.iter().find(|(name, _)| s.starts_with(name)) {
            s = &s[name.len()..];
            for (p, id) in [("TH", S_TH), ("th", S_LTH), ("SP", S_SP)] {
                if let Some(r) = s.strip_prefix(p) {
                    suffix |= id;
                    s = r;
                    break;
                }
            }
            out.push(Node::Action(*k, suffix));
            continue;
        }
        if let Some(r) = s.strip_prefix('"') {
            // quoted text: a backslash quotes the next character
            let mut chars = r.char_indices().peekable();
            let mut text = String::new();
            let mut end = r.len();
            while let Some((i, c)) = chars.next() {
                if c == '"' {
                    end = i + 1;
                    break;
                }
                if c == '\\'
                    && let Some(&(_, next)) = chars.peek()
                {
                    chars.next();
                    text.push(next);
                    continue;
                }
                text.push(c);
            }
            out.push(Node::Text(text));
            s = &r[end.min(r.len())..];
            continue;
        }
        // (outside quotes a backslash is special only before a double quote)
        if let Some(r) = s.strip_prefix("\\\"") {
            out.push(Node::Text("\"".into()));
            s = r;
            continue;
        }
        let c = s.chars().next().unwrap_or(' ');
        out.push(Node::Text(c.to_string()));
        s = &s[c.len_utf8()..];
    }
    out
}

/// The fields to_char writes from (its fmt_tm), and the zone's offset east and abbreviation for a timestamptz.
struct CharTm {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
    fsec: i64,
    wday: i64,
    yday: i64,
    gmtoff: i64,
    tzn: Option<String>,
}

/// date2j, as the C computes it (for any fields, an interval's too).
fn date2j(year: i64, month: i64, day: i64) -> i64 {
    let (y, m) = if month > 2 { (year + 4800, month + 1) } else { (year + 4799, month + 13) };
    let century = y / 100;
    y * 365 - 32167 + y / 4 - century + century / 4 + 7834 * m / 256 + day
}

/// j2day: the day of the week of a Julian day, Sunday 0.
fn j2day(jd: i64) -> i64 {
    (jd + 1).rem_euclid(7)
}

fn isoweek2j(year: i64, week: i64) -> i64 {
    let day4 = date2j(year, 1, 4);
    let day0 = j2day(day4 - 1);
    (week - 1) * 7 + (day4 - day0)
}

/// date2isoweek: the ISO 8601 week of the year.
fn date2isoweek(year: i64, mon: i64, mday: i64) -> i64 {
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4 - 1);
    if dayn < day4 - day0 {
        day4 = date2j(year - 1, 1, 4);
        day0 = j2day(day4 - 1);
    }
    let mut result = (dayn - (day4 - day0)) / 7 + 1;
    if result >= 52 {
        day4 = date2j(year + 1, 1, 4);
        day0 = j2day(day4 - 1);
        if dayn >= day4 - day0 {
            result = (dayn - (day4 - day0)) / 7 + 1;
        }
    }
    result
}

/// date2isoyear: the ISO 8601 year (with a year zero).
fn date2isoyear(year: i64, mon: i64, mday: i64) -> i64 {
    let mut year = year;
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4 - 1);
    if dayn < day4 - day0 {
        day4 = date2j(year - 1, 1, 4);
        day0 = j2day(day4 - 1);
        year -= 1;
    }
    let result = (dayn - (day4 - day0)) / 7 + 1;
    if result >= 52 {
        day4 = date2j(year + 1, 1, 4);
        day0 = j2day(day4 - 1);
        if dayn >= day4 - day0 {
            year += 1;
        }
    }
    year
}

/// date2isoyearday: the ISO 8601 day of the year, 1 to 371.
fn date2isoyearday(year: i64, mon: i64, mday: i64) -> i64 {
    date2j(year, mon, mday) - isoweek2j(date2isoyear(year, mon, mday), 1) + 1
}

/// get_th: the English ordinal suffix of a written number (teens take th).
fn th(num: &str, upper: bool) -> Result<&'static str, String> {
    let b = num.as_bytes();
    let Some(&last) = b.last().filter(|c| c.is_ascii_digit()) else { return Err(format!("\"{num}\" is not a number")) };
    let last = if b.len() > 1 && b[b.len() - 2] == b'1' { b'0' } else { last };
    Ok(match (last, upper) {
        (b'1', true) => "ST",
        (b'1', false) => "st",
        (b'2', true) => "ND",
        (b'2', false) => "nd",
        (b'3', true) => "RD",
        (b'3', false) => "rd",
        (_, true) => "TH",
        (_, false) => "th",
    })
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
const RM_UPPER: [&str; 12] = ["XII", "XI", "X", "IX", "VIII", "VII", "VI", "V", "IV", "III", "II", "I"];
const RM_LOWER: [&str; 12] = ["xii", "xi", "x", "ix", "viii", "vii", "vi", "v", "iv", "iii", "ii", "i"];

/// Why a template doesn't suit the value: its message and hint.
pub type CharErr = (String, Option<&'static str>);

/// DCH_to_char.
fn dch(fmt: &str, tm: &CharTm, is_interval: bool) -> Result<String, CharErr> {
    let invalid = || -> CharErr {
        (
            "invalid format specification for an interval value".into(),
            Some("Intervals are not tied to specific calendar dates."),
        )
    };
    let adjust = |y: i64| if is_interval || y > 0 { y } else { -(y - 1) };
    let mut out = String::new();
    for node in parse_format(fmt) {
        let (k, suf) = match node {
            Node::Text(t) => {
                out += &t;
                continue;
            }
            Node::Action(k, s) => (k, s),
        };
        let fm = suf & S_FM != 0;
        let tm_ = suf & S_TM != 0;
        // a number: zero-padded to `width` (one more when negative) unless FM, then its ordinal suffix
        let num = |n: i64, width: usize| -> Result<String, CharErr> {
            let w = if fm {
                0
            } else if n >= 0 {
                width
            } else {
                width + 1
            };
            let mut s = format!("{n:0w$}");
            if suf & (S_TH | S_LTH) != 0 {
                let o = th(&s, suf & S_TH != 0).map_err(|m| (m, None))?;
                s.push_str(o);
            }
            Ok(s)
        };
        // a word: padded to nine unless FM or TM
        let word = |w: String| if fm || tm_ { w } else { format!("{w:<9}") };
        let pm = tm.hour.rem_euclid(24) >= 12;
        let year = adjust(tm.year);
        let iso = || adjust(date2isoyear(tm.year, tm.mon, tm.mday));
        let piece = match k {
            K::AM_ | K::PM_ => (if pm { "P.M." } else { "A.M." }).to_string(),
            K::AM | K::PM => (if pm { "PM" } else { "AM" }).to_string(),
            K::am_ | K::pm_ => (if pm { "p.m." } else { "a.m." }).to_string(),
            K::am | K::pm => (if pm { "pm" } else { "am" }).to_string(),
            K::HH | K::HH12 => num(if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 }, 2)?,
            K::HH24 => num(tm.hour, 2)?,
            K::MI => num(tm.min, 2)?,
            K::SS => num(tm.sec, 2)?,
            K::FF(n) => {
                let digits = usize::from(n);
                let v = tm.fsec / 10i64.pow(6 - u32::from(n));
                let mut s = format!("{v:0digits$}");
                if suf & (S_TH | S_LTH) != 0 {
                    let o = th(&s, suf & S_TH != 0).map_err(|m| (m, None))?;
                    s.push_str(o);
                }
                s
            }
            K::MS => {
                let mut s = format!("{:03}", tm.fsec / 1000);
                if suf & (S_TH | S_LTH) != 0 {
                    let o = th(&s, suf & S_TH != 0).map_err(|m| (m, None))?;
                    s.push_str(o);
                }
                s
            }
            K::US => {
                let mut s = format!("{:06}", tm.fsec);
                if suf & (S_TH | S_LTH) != 0 {
                    let o = th(&s, suf & S_TH != 0).map_err(|m| (m, None))?;
                    s.push_str(o);
                }
                s
            }
            K::SSSS => {
                let mut s = (tm.hour * 3600 + tm.min * 60 + tm.sec).to_string();
                if suf & (S_TH | S_LTH) != 0 {
                    let o = th(&s, suf & S_TH != 0).map_err(|m| (m, None))?;
                    s.push_str(o);
                }
                s
            }
            K::tz | K::TZ => {
                if is_interval {
                    return Err(invalid());
                }
                match &tm.tzn {
                    Some(z) if k == K::tz => z.to_ascii_lowercase(),
                    Some(z) => z.clone(),
                    None => String::new(),
                }
            }
            K::TZH => {
                if is_interval {
                    return Err(invalid());
                }
                format!("{}{:02}", if tm.gmtoff >= 0 { '+' } else { '-' }, tm.gmtoff.abs() / 3600)
            }
            K::TZM => {
                if is_interval {
                    return Err(invalid());
                }
                format!("{:02}", tm.gmtoff.abs() % 3600 / 60)
            }
            K::OF => {
                if is_interval {
                    return Err(invalid());
                }
                let w = if fm { 0 } else { 2 };
                let mut s = format!("{}{:0w$}", if tm.gmtoff >= 0 { '+' } else { '-' }, tm.gmtoff.abs() / 3600);
                if tm.gmtoff.abs() % 3600 != 0 {
                    s += &format!(":{:02}", tm.gmtoff.abs() % 3600 / 60);
                }
                s
            }
            K::AD_ | K::BC_ | K::AD | K::BC | K::ad_ | K::bc_ | K::ad | K::bc => {
                if is_interval {
                    return Err(invalid());
                }
                let bce = tm.year <= 0;
                (match k {
                    K::AD_ | K::BC_ => {
                        if bce {
                            "B.C."
                        } else {
                            "A.D."
                        }
                    }
                    K::AD | K::BC => {
                        if bce {
                            "BC"
                        } else {
                            "AD"
                        }
                    }
                    K::ad_ | K::bc_ => {
                        if bce {
                            "b.c."
                        } else {
                            "a.d."
                        }
                    }
                    _ => {
                        if bce {
                            "bc"
                        } else {
                            "ad"
                        }
                    }
                })
                .to_string()
            }
            K::MONTH | K::Month | K::month | K::MON | K::Mon | K::mon => {
                if is_interval {
                    return Err(invalid());
                }
                if tm.mon == 0 {
                    continue;
                }
                let full = MONTHS[(tm.mon - 1).clamp(0, 11) as usize];
                match k {
                    K::MONTH => word(full.to_uppercase()),
                    K::Month => word(full.to_string()),
                    K::month => word(full.to_lowercase()),
                    K::MON => full[..3].to_uppercase(),
                    K::Mon => full[..3].to_string(),
                    _ => full[..3].to_lowercase(),
                }
            }
            K::MM => num(tm.mon, 2)?,
            K::DAY | K::Day | K::day | K::DY | K::Dy | K::dy => {
                if is_interval {
                    return Err(invalid());
                }
                let full = DAYS[tm.wday.rem_euclid(7) as usize];
                match k {
                    K::DAY => word(full.to_uppercase()),
                    K::Day => word(full.to_string()),
                    K::day => word(full.to_lowercase()),
                    K::DY => full[..3].to_uppercase(),
                    K::Dy => full[..3].to_string(),
                    _ => full[..3].to_lowercase(),
                }
            }
            K::DDD => num(tm.yday, 3)?,
            K::IDDD => num(date2isoyearday(tm.year, tm.mon, tm.mday), 3)?,
            K::DD => num(tm.mday, 2)?,
            K::D => {
                if is_interval {
                    return Err(invalid());
                }
                num(tm.wday + 1, 0)?
            }
            K::ID => {
                if is_interval {
                    return Err(invalid());
                }
                num(if tm.wday == 0 { 7 } else { tm.wday }, 0)?
            }
            K::WW => num((tm.yday - 1) / 7 + 1, 2)?,
            K::IW => num(date2isoweek(tm.year, tm.mon, tm.mday), 2)?,
            K::Q => {
                if tm.mon == 0 {
                    continue;
                }
                num((tm.mon - 1) / 3 + 1, 0)?
            }
            K::CC => {
                let c = if is_interval {
                    tm.year / 100
                } else if tm.year > 0 {
                    (tm.year - 1) / 100 + 1 // (century 20 is 1901 to 2000)
                } else {
                    tm.year / 100 - 1
                };
                if (-99..=99).contains(&c) {
                    num(c, 2)?
                } else {
                    let mut s = c.to_string();
                    if suf & (S_TH | S_LTH) != 0 {
                        let o = th(&s, suf & S_TH != 0).map_err(|m| (m, None))?;
                        s.push_str(o);
                    }
                    s
                }
            }
            K::Y_YYY => {
                let i = year / 1000;
                let mut s = format!("{i},{:03}", year - i * 1000);
                if suf & (S_TH | S_LTH) != 0 {
                    let o = th(&s, suf & S_TH != 0).map_err(|m| (m, None))?;
                    s.push_str(o);
                }
                s
            }
            K::YYYY => num(year, 4)?,
            K::IYYY => num(iso(), 4)?,
            K::YYY => num_width(year, iso_width(year, 3), year % 1000, fm, suf)?,
            K::IYY => {
                let y = iso();
                num_width(y, iso_width(y, 3), y % 1000, fm, suf)?
            }
            K::YY => num_width(year, iso_width(year, 2), year % 100, fm, suf)?,
            K::IY => {
                let y = iso();
                num_width(y, iso_width(y, 2), y % 100, fm, suf)?
            }
            K::Y => num_width(year, 1, year % 10, true, suf)?,
            K::I => {
                let y = iso();
                num_width(y, 1, y % 10, true, suf)?
            }
            K::RM | K::rm => {
                if tm.mon == 0 && tm.year == 0 {
                    continue;
                }
                let at = if tm.mon == 0 {
                    if tm.year >= 0 { 0 } else { 11 }
                } else if tm.mon < 0 {
                    -(tm.mon + 1)
                } else {
                    12 - tm.mon
                };
                let names = if k == K::RM { &RM_UPPER } else { &RM_LOWER };
                let r = names[at.clamp(0, 11) as usize];
                if fm { r.to_string() } else { format!("{r:<4}") }
            }
            K::W => num((tm.mday - 1) / 7 + 1, 0)?,
            K::J => num(date2j(tm.year, tm.mon, tm.mday), 0)?,
            K::FX => String::new(),
        };
        out += &piece;
    }
    Ok(out)
}

/// The width a part of a year is padded to: one more when the whole year is negative (as `%0*d` with the
/// year's sign decides it).
fn iso_width(year: i64, width: usize) -> usize {
    if year >= 0 { width } else { width + 1 }
}

/// `value` zero-padded to `width` (none with FM), then its ordinal suffix.
fn num_width(_year: i64, width: usize, value: i64, fm: bool, suf: u8) -> Result<String, CharErr> {
    let w = if fm { 0 } else { width };
    let mut s = format!("{value:0w$}");
    if suf & (S_TH | S_LTH) != 0 {
        let o = th(&s, suf & S_TH != 0).map_err(|m| (m, None))?;
        s.push_str(o);
    }
    Ok(s)
}

/// `to_char(timestamp, fmt)`: `us` is the wall clock (in the session's zone for a timestamptz), `zone` the
/// timestamptz's offset east of UTC in seconds and its abbreviation.
pub fn to_char(us: i64, fmt: &str, zone: Option<(i32, &str)>) -> Result<String, CharErr> {
    let (day, t) = (us.div_euclid(DAY), us.rem_euclid(DAY));
    let (y, m, d) = civil_from_days(day);
    let jd = day + 2_440_588;
    let tm = CharTm {
        year: y,
        mon: i64::from(m),
        mday: i64::from(d),
        hour: t / HOUR,
        min: t % HOUR / MINUTE,
        sec: t % MINUTE / SECOND,
        fsec: t % SECOND,
        wday: (jd + 1).rem_euclid(7),
        yday: day - days_from_civil(y, 1, 1) + 1,
        gmtoff: zone.map_or(0, |(o, _)| i64::from(o)),
        tzn: zone.map(|(_, n)| n.to_string()),
    };
    dch(fmt, &tm, false)
}

/// `to_char(interval, fmt)`: years, months and days as the interval keeps them, the time as hours (not wrapped at
/// a day) and the rest; the day of the year is the whole span in days, a month being 30.
pub fn to_char_interval(i: Interval, fmt: &str) -> Result<String, CharErr> {
    let t = i.micros;
    let (year, mon) = (i64::from(i.months / 12), i64::from(i.months % 12));
    let tm = CharTm {
        year,
        mon,
        mday: i64::from(i.days),
        hour: t / HOUR,
        min: t % HOUR / MINUTE,
        sec: t % MINUTE / SECOND,
        fsec: t % SECOND,
        wday: 0,
        yday: (year * 12 + mon) * 30 + i64::from(i.days),
        gmtoff: 0,
        tzn: None,
    };
    dch(fmt, &tm, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i64, m: u32, d: u32, h: i64, mi: i64, s: i64) -> i64 {
        days_from_civil(y, m, d) * DAY + h * HOUR + mi * MINUTE + s * SECOND
    }

    #[test]
    fn writes_as_postgres() {
        let t = at(1997, 2, 10, 17, 32, 1);
        let f = |fmt: &str| to_char(t, fmt, None).unwrap();
        assert_eq!(
            f("DAY Day day DY Dy dy MONTH Month month RM MON Mon mon"),
            "MONDAY    Monday    monday    MON Mon mon FEBRUARY  February  february  II   FEB Feb feb"
        );
        assert_eq!(f("Y,YYY YYYY YYY YY Y CC Q MM WW DDD DD D J"), "1,997 1997 997 97 7 20 1 02 06 041 10 2 2450490");
        assert_eq!(f("YYYYTH YYYYth Jth"), "1997TH 1997th 2450490th");
        assert_eq!(f("HH HH12 HH24 MI SS SSSS"), "05 05 17 32 01 63121");
        assert_eq!(
            f("\"HH:MI:SS is\" HH:MI:SS \"\\\"text between quote marks\\\"\""),
            "HH:MI:SS is 05:32:01 \"text between quote marks\""
        );
        assert_eq!(f("IYYY IYY IY I IW IDDD ID"), "1997 997 97 7 07 043 1");
        assert_eq!(f("FMMonth FMDD"), "February 10");
        assert_eq!(f("OF"), "+00");
        let bc = at(-96, 2, 16, 17, 32, 1); // 97 BC
        assert_eq!(to_char(bc, "Y,YYY YYYY CC BC", None).unwrap(), "0,097 0097 -01 BC");
    }

    #[test]
    fn writes_intervals() {
        let i = Interval { months: 14, days: 3, micros: 4 * HOUR + 5 * MINUTE + 6 * SECOND };
        assert_eq!(to_char_interval(i, "YYYY MM DD HH24:MI:SS RM").unwrap(), "0001 02 03 04:05:06 II  ");
        assert!(to_char_interval(i, "Day").is_err());
    }
}
