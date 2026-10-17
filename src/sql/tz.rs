//! Named time zones with daylight saving time, read from the system's tz database (`/usr/share/zoneinfo`,
//! or `TZDIR`) the way Postgres built with `--with-system-tzdata` reads it: a TZif file's transitions, and
//! for the years after its last one the POSIX rule in its footer (`EST5EDT,M3.2.0,M11.1.0`). Nothing here
//! knows SQL: a zone answers two questions, what its offset is at an instant and which instant a wall
//! clock reading is.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

/// One kind of time a zone keeps: its offset east of UTC, whether it is daylight saving time, and its
/// abbreviation.
#[derive(Clone, Debug, PartialEq)]
pub struct Kind {
    pub off: i32,
    pub dst: bool,
    pub abbr: String,
}

/// A day of the year a POSIX rule names.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Day {
    /// `Mm.w.d`: day `d` (0 is Sunday) of week `w` (5 is the last) of month `m`
    Month(u8, u8, u8),
    /// `Jn`: day 1 to 365, February 29th not counted
    Julian(u16),
    /// `n`: day 0 to 365, February 29th counted
    Zero(u16),
}

/// The rule for the years after a zone's last transition: standard time, and maybe a daylight saving time
/// with its start and end (as local seconds after midnight, which can be negative or past 24 hours).
#[derive(Clone, Debug, PartialEq)]
struct Posix {
    std: Kind,
    dst: Option<(Kind, Day, i32, Day, i32)>,
}

/// A zone: its name as the tz database spells it, its transitions and its footer rule.
#[derive(Debug)]
pub struct Rules {
    pub name: String,
    ats: Vec<i64>,
    idx: Vec<u8>,
    kinds: Vec<Kind>,
    footer: Option<Posix>,
    /// a dynamic abbreviation (`MSK`): what it meant in its zone at each moment (see `dynamic_abbrev`)
    abbr_of: Option<(Arc<Rules>, String)>,
}

impl PartialEq for Rules {
    fn eq(&self, other: &Rules) -> bool {
        self.name == other.name
    }
}

/// What Postgres's default `timezone_abbreviations` set calls each abbreviation: (name, seconds east of UTC, is it
/// daylight saving time). `AT TIME ZONE 'PST'` and a timestamp written `... EST` take them; `SET TIME ZONE` does not.
const ABBREVS: &[(&str, i32, bool)] = &[
    ("ACDT", 37800, true),
    ("ACSST", 37800, true),
    ("ACST", 34200, false),
    ("ACT", -18000, false),
    ("ACWST", 31500, false),
    ("ADT", -10800, true),
    ("AEDT", 39600, true),
    ("AESST", 39600, true),
    ("AEST", 36000, false),
    ("AFT", 16200, false),
    ("AKDT", -28800, true),
    ("AKST", -32400, false),
    ("ALMST", 25200, true),
    ("ALMT", 21600, false),
    ("AMST", 14400, false),
    ("AMT", -14400, false),
    ("ANAST", 43200, false),
    ("ANAT", 43200, false),
    ("ARST", -10800, false),
    ("ART", -10800, false),
    ("AST", -14400, false),
    ("AWSST", 32400, true),
    ("AWST", 28800, false),
    ("AZOST", 0, true),
    ("AZOT", -3600, false),
    ("AZST", 14400, false),
    ("AZT", 14400, false),
    ("BDST", 7200, true),
    ("BDT", 21600, false),
    ("BNT", 28800, false),
    ("BORT", 28800, false),
    ("BOT", -14400, false),
    ("BRA", -10800, false),
    ("BRST", -7200, true),
    ("BRT", -10800, false),
    ("BST", 3600, true),
    ("BTT", 21600, false),
    ("CADT", 37800, true),
    ("CAST", 34200, false),
    ("CCT", 28800, false),
    ("CDT", -18000, true),
    ("CEST", 7200, true),
    ("CET", 3600, false),
    ("CETDST", 7200, true),
    ("CHADT", 49500, true),
    ("CHAST", 45900, false),
    ("CHUT", 36000, false),
    ("CKT", -36000, false),
    ("CLST", -10800, true),
    ("CLT", -10800, true),
    ("COT", -18000, false),
    ("CST", -21600, false),
    ("CXT", 25200, false),
    ("DAVT", 25200, false),
    ("DDUT", 36000, false),
    ("EASST", -18000, true),
    ("EAST", -18000, true),
    ("EAT", 10800, false),
    ("EDT", -14400, true),
    ("EEST", 10800, true),
    ("EET", 7200, false),
    ("EETDST", 10800, true),
    ("EGST", 0, true),
    ("EGT", -3600, false),
    ("EST", -18000, false),
    ("FET", 10800, false),
    ("FJST", 46800, true),
    ("FJT", 43200, false),
    ("FKST", -10800, false),
    ("FKT", -10800, false),
    ("FNST", -3600, true),
    ("FNT", -7200, false),
    ("GALT", -21600, false),
    ("GAMT", -32400, false),
    ("GEST", 14400, false),
    ("GET", 14400, false),
    ("GFT", -10800, false),
    ("GILT", 43200, false),
    ("GMT", 0, false),
    ("GYT", -14400, false),
    ("HKT", 28800, false),
    ("HST", -36000, false),
    ("ICT", 25200, false),
    ("IDT", 10800, true),
    ("IOT", 21600, false),
    ("IRKST", 28800, false),
    ("IRKT", 28800, false),
    ("IRT", 12600, false),
    ("IST", 7200, false),
    ("JAYT", 32400, false),
    ("JST", 32400, false),
    ("KDT", 36000, true),
    ("KGST", 21600, true),
    ("KGT", 21600, false),
    ("KOST", 39600, false),
    ("KRAST", 25200, false),
    ("KRAT", 25200, false),
    ("KST", 32400, false),
    ("LHDT", 37800, false),
    ("LHST", 37800, false),
    ("LIGT", 36000, false),
    ("LINT", 50400, false),
    ("LKT", 19800, false),
    ("MAGST", 39600, false),
    ("MAGT", 39600, false),
    ("MART", -34200, false),
    ("MAWT", 18000, false),
    ("MDT", -21600, true),
    ("MEST", 7200, true),
    ("MESZ", 7200, true),
    ("MET", 3600, false),
    ("METDST", 7200, true),
    ("MEZ", 3600, false),
    ("MHT", 43200, false),
    ("MMT", 23400, false),
    ("MPT", 36000, false),
    ("MSD", 14400, true),
    ("MSK", 10800, false),
    ("MST", -25200, false),
    ("MUST", 18000, true),
    ("MUT", 14400, false),
    ("MVT", 18000, false),
    ("MYT", 28800, false),
    ("NDT", -9000, true),
    ("NFT", -12600, false),
    ("NOVST", 25200, false),
    ("NOVT", 25200, false),
    ("NPT", 20700, false),
    ("NST", -12600, false),
    ("NUT", -39600, false),
    ("NZDT", 46800, true),
    ("NZST", 43200, false),
    ("NZT", 43200, false),
    ("OMSST", 21600, false),
    ("OMST", 21600, false),
    ("PDT", -25200, true),
    ("PET", -18000, false),
    ("PETST", 43200, false),
    ("PETT", 43200, false),
    ("PGT", 36000, false),
    ("PHT", 28800, false),
    ("PKST", 21600, true),
    ("PKT", 18000, false),
    ("PMDT", -7200, true),
    ("PMST", -10800, false),
    ("PONT", 39600, false),
    ("PST", -28800, false),
    ("PWT", 32400, false),
    ("PYST", -10800, true),
    ("PYT", -10800, false),
    ("RET", 14400, false),
    ("SADT", 37800, true),
    ("SAST", 7200, false),
    ("SCT", 14400, false),
    ("SGT", 28800, false),
    ("TAHT", -36000, false),
    ("TFT", 18000, false),
    ("TJT", 18000, false),
    ("TKT", 46800, false),
    ("TMT", 18000, false),
    ("TOT", 46800, false),
    ("TRUT", 36000, false),
    ("TVT", 43200, false),
    ("UCT", 0, false),
    ("ULAST", 32400, true),
    ("ULAT", 28800, false),
    ("UT", 0, false),
    ("UTC", 0, false),
    ("UYST", -7200, true),
    ("UYT", -10800, false),
    ("UZST", 21600, true),
    ("UZT", 18000, false),
    ("VET", -14400, false),
    ("VLAST", 36000, false),
    ("VLAT", 36000, false),
    ("VOLT", 10800, false),
    ("VUT", 39600, false),
    ("WADT", 28800, true),
    ("WAKT", 43200, false),
    ("WAST", 25200, false),
    ("WAT", 3600, false),
    ("WDT", 32400, true),
    ("WET", 0, false),
    ("WETDST", 3600, true),
    ("WFT", 43200, false),
    ("WGST", -7200, true),
    ("WGT", -10800, false),
    ("XJT", 21600, false),
    ("YAKST", 32400, false),
    ("YAKT", 32400, false),
    ("YAPT", 36000, false),
    ("YEKST", 21600, true),
    ("YEKT", 18000, false),
    ("Z", 0, false),
    ("ZULU", 0, false),
];

/// Every abbreviation: (name, seconds east of UTC, is it daylight saving time).
pub fn abbrevs() -> &'static [(&'static str, i32, bool)] {
    ABBREVS
}

/// A zone abbreviation's offset, in seconds east of UTC.
pub fn abbrev(name: &str) -> Option<(i32, bool)> {
    ABBREVS.iter().find(|(a, ..)| a.eq_ignore_ascii_case(name.trim())).map(|(_, o, d)| (*o, *d))
}

// ---------------------------------------------------------------- the database

fn dirs() -> Vec<String> {
    let mut d = vec![];
    if let Ok(z) = std::env::var("TZDIR")
        && !z.is_empty()
    {
        d.push(z);
    }
    for z in ["/usr/share/zoneinfo", "/usr/lib/zoneinfo", "/usr/share/lib/zoneinfo", "/etc/zoneinfo"] {
        d.push(z.into());
    }
    d
}

/// Every zone the database has: lower-cased name to name as spelled. Empty when there is no database.
fn names() -> &'static BTreeMap<String, String> {
    static NAMES: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut out = BTreeMap::new();
        for root in dirs() {
            let root = std::path::PathBuf::from(root);
            if !root.is_dir() {
                continue;
            }
            let mut stack = vec![(root.clone(), String::new())];
            while let Some((dir, prefix)) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                for e in rd.flatten() {
                    let file = e.file_name().to_string_lossy().into_owned();
                    let name = if prefix.is_empty() { file.clone() } else { format!("{prefix}/{file}") };
                    let path = e.path();
                    if path.is_dir() {
                        // (posix/ and right/ are the same zones again, with and without leap seconds)
                        if prefix.is_empty() && matches!(file.as_str(), "posix" | "right") {
                            continue;
                        }
                        stack.push((path, name));
                    } else if !file.contains('.') && std::fs::read(&path).is_ok_and(|b| b.starts_with(b"TZif")) {
                        out.insert(name.to_lowercase(), name);
                    }
                }
            }
            if !out.is_empty() {
                break;
            }
        }
        out
    })
}

/// Every zone's name, in order.
pub fn all_names() -> Vec<String> {
    names().values().cloned().collect()
}

/// A zone by name, in any case; None if the database doesn't have it (or has no database).
pub fn find(name: &str) -> Option<Arc<Rules>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<Rules>>>>> = OnceLock::new();
    let key = name.trim().to_lowercase();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().ok()?.get(&key) {
        return hit.clone();
    }
    let rules = names().get(&key).and_then(|canonical| {
        dirs().into_iter().find_map(|d| {
            let bytes = std::fs::read(std::path::Path::new(&d).join(canonical)).ok()?;
            parse(canonical, &bytes)
        })
    });
    let rules = rules.map(Arc::new);
    cache.lock().ok()?.insert(key, rules.clone());
    rules
}

/// A zone written as a POSIX rule (`CST7CDT,M4.1.0,M10.5.0`, `EST5EDT`, `FOO8BAR`, `GMT+8`), as Postgres's pg_tzset
/// takes one the database doesn't name; None if it isn't one.
pub fn from_posix(spec: &str) -> Option<Arc<Rules>> {
    let p = posix(spec.trim())?;
    Some(Arc::new(Rules {
        name: spec.trim().to_string(),
        ats: vec![],
        idx: vec![],
        kinds: vec![p.std.clone()],
        footer: Some(p),
        abbr_of: None,
    }))
}

/// The abbreviations of Postgres's default set that stand for a zone rather than an offset (`MSK Europe/Moscow`):
/// what each means is what it meant in that zone at the time in question.
const DYNAMIC: &[(&str, &str)] = &[
    ("AMST", "Asia/Yerevan"),
    ("ANAST", "Asia/Anadyr"),
    ("ANAT", "Asia/Anadyr"),
    ("ARST", "America/Argentina/Buenos_Aires"),
    ("ART", "America/Argentina/Buenos_Aires"),
    ("AZST", "Asia/Baku"),
    ("AZT", "Asia/Baku"),
    ("CKT", "Pacific/Rarotonga"),
    ("CLT", "America/Santiago"),
    ("DAVT", "Antarctica/Davis"),
    ("EASST", "Pacific/Easter"),
    ("EAST", "Pacific/Easter"),
    ("FKST", "Atlantic/Stanley"),
    ("FKT", "Atlantic/Stanley"),
    ("GEST", "Asia/Tbilisi"),
    ("GET", "Asia/Tbilisi"),
    ("GYT", "America/Guyana"),
    ("IOT", "Indian/Chagos"),
    ("IRKST", "Asia/Irkutsk"),
    ("IRKT", "Asia/Irkutsk"),
    ("KGT", "Asia/Bishkek"),
    ("KOST", "Pacific/Kosrae"),
    ("KRAST", "Asia/Krasnoyarsk"),
    ("KRAT", "Asia/Krasnoyarsk"),
    ("LHDT", "Australia/Lord_Howe"),
    ("LINT", "Pacific/Kiritimati"),
    ("LKT", "Asia/Colombo"),
    ("MAGST", "Asia/Magadan"),
    ("MAGT", "Asia/Magadan"),
    ("MAWT", "Antarctica/Mawson"),
    ("MSK", "Europe/Moscow"),
    ("NOVST", "Asia/Novosibirsk"),
    ("NOVT", "Asia/Novosibirsk"),
    ("NUT", "Pacific/Niue"),
    ("OMSST", "Asia/Omsk"),
    ("OMST", "Asia/Omsk"),
    ("PETST", "Asia/Kamchatka"),
    ("PETT", "Asia/Kamchatka"),
    ("PYT", "America/Asuncion"),
    ("SGT", "Asia/Singapore"),
    ("TKT", "Pacific/Fakaofo"),
    ("TMT", "Asia/Ashgabat"),
    ("ULAT", "Asia/Ulaanbaatar"),
    ("VET", "America/Caracas"),
    ("VLAST", "Asia/Vladivostok"),
    ("VLAT", "Asia/Vladivostok"),
    ("VOLT", "Europe/Volgograd"),
    ("YAKST", "Asia/Yakutsk"),
    ("YAKT", "Asia/Yakutsk"),
    ("YEKT", "Asia/Yekaterinburg"),
];

/// A dynamic abbreviation (see `DYNAMIC`), in any case, as a zone whose offset at each moment is the one the
/// abbreviation had then (pg_interpret_timezone_abbrev); None for any other, or without the tz database's zone
/// (the fixed offset of `ABBREVS` is then its meaning).
pub fn dynamic_abbrev(name: &str) -> Option<Arc<Rules>> {
    let upper = name.trim().to_ascii_uppercase();
    let (_, zone) = DYNAMIC.iter().find(|(a, _)| *a == upper)?;
    let zone = find(zone)?;
    Some(Arc::new(Rules {
        name: upper.clone(),
        ats: vec![],
        idx: vec![],
        kinds: vec![],
        footer: None,
        abbr_of: Some((zone, upper)),
    }))
}

// ---------------------------------------------------------------- TZif

fn be32(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A TZif file: the version 2 (64-bit) block when there is one, else the version 1 block.
fn parse(name: &str, b: &[u8]) -> Option<Rules> {
    if !b.starts_with(b"TZif") {
        return None;
    }
    let version = *b.get(4)?;
    let counts = |at: usize| -> Option<[usize; 6]> {
        let mut c = [0usize; 6];
        for (i, v) in c.iter_mut().enumerate() {
            *v = usize::try_from(be32(b, at + 20 + 4 * i)?).ok()?;
        }
        Some(c)
    };
    // the counts: isutc, isstd, leap, time, type, char
    let c1 = counts(0)?;
    let v1_len = c1[3] * 4 + c1[3] + c1[4] * 6 + c1[5] + c1[2] * 8 + c1[1] + c1[0];
    let (start, c, wide) = if version >= b'2' {
        let at = 44 + v1_len;
        if !b.get(at..at + 4)?.starts_with(b"TZif") {
            return None;
        }
        (at + 44, counts(at)?, true)
    } else {
        (44, c1, false)
    };
    let [isut, isstd, leap, timecnt, typecnt, charcnt] = c;
    let tsize = if wide { 8 } else { 4 };
    let mut at = start;
    let mut ats = Vec::with_capacity(timecnt);
    for i in 0..timecnt {
        ats.push(if wide { be64(b, at + i * 8)? } else { i64::from(be32(b, at + i * 4)?) });
    }
    at += timecnt * tsize;
    let idx = b.get(at..at + timecnt)?.to_vec();
    at += timecnt;
    let ttinfo = b.get(at..at + typecnt * 6)?;
    at += typecnt * 6;
    let chars = b.get(at..at + charcnt)?;
    at += charcnt;
    let abbr = |i: usize| -> String {
        let s = chars.get(i..).unwrap_or_default();
        String::from_utf8_lossy(&s[..s.iter().position(|c| *c == 0).unwrap_or(s.len())]).into_owned()
    };
    let kinds: Vec<Kind> = (0..typecnt)
        .map(|i| {
            let t = &ttinfo[i * 6..i * 6 + 6];
            Kind { off: i32::from_be_bytes([t[0], t[1], t[2], t[3]]), dst: t[4] != 0, abbr: abbr(usize::from(t[5])) }
        })
        .collect();
    if kinds.is_empty() || idx.iter().any(|i| usize::from(*i) >= kinds.len()) {
        return None;
    }
    let footer = if wide {
        at += leap * (tsize + 4) + isstd + isut;
        let f = b.get(at..)?;
        let f = f.strip_prefix(b"\n")?;
        let end = f.iter().position(|c| *c == b'\n')?;
        let s = std::str::from_utf8(&f[..end]).ok()?;
        if s.is_empty() { None } else { posix(s) }
    } else {
        None
    };
    Some(Rules { name: name.to_string(), ats, idx, kinds, footer, abbr_of: None })
}

// ---------------------------------------------------------------- POSIX rules

/// A zone abbreviation in a POSIX string: letters, or anything in angle brackets.
fn posix_abbr(s: &str) -> Option<(String, &str)> {
    if let Some(r) = s.strip_prefix('<') {
        let end = r.find('>')?;
        return Some((r[..end].to_string(), &r[end + 1..]));
    }
    let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
    (end >= 3).then(|| (s[..end].to_string(), &s[end..]))
}

/// `[+-]hh[:mm[:ss]]` in seconds, and what follows it.
fn posix_time(s: &str) -> Option<(i32, &str)> {
    let (sign, s) = match s.as_bytes().first()? {
        b'-' => (-1, &s[1..]),
        b'+' => (1, &s[1..]),
        _ => (1, s),
    };
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == ':')).unwrap_or(s.len());
    let mut parts = s[..end].split(':');
    let h: i32 = parts.next()?.parse().ok()?;
    let m: i32 = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    let sec: i32 = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    Some((sign * (h * 3600 + m * 60 + sec), &s[end..]))
}

fn posix_day(s: &str) -> Option<(Day, &str)> {
    let end = s.find(['/', ',']).unwrap_or(s.len());
    let (d, rest) = s.split_at(end);
    let day = if let Some(m) = d.strip_prefix('M') {
        let mut p = m.split('.');
        let (mo, w, dd): (u8, u8, u8) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?);
        ((1..=12).contains(&mo) && (1..=5).contains(&w) && dd <= 6).then_some(Day::Month(mo, w, dd))?
    } else if let Some(j) = d.strip_prefix('J') {
        Day::Julian(j.parse().ok().filter(|n| (1..=365).contains(n))?)
    } else {
        Day::Zero(d.parse().ok().filter(|n| *n <= 365)?)
    };
    Some((day, rest))
}

fn posix(s: &str) -> Option<Posix> {
    let (std_abbr, rest) = posix_abbr(s)?;
    let (west, rest) = posix_time(rest)?;
    let std = Kind { off: -west, dst: false, abbr: std_abbr };
    if rest.is_empty() {
        return Some(Posix { std, dst: None });
    }
    let (dst_abbr, rest) = posix_abbr(rest)?;
    let (dst_off, rest) = match rest.starts_with(',') || rest.is_empty() {
        true => (std.off + 3600, rest),
        false => {
            let (w, r) = posix_time(rest)?;
            (-w, r)
        }
    };
    // (the default rule, when there is none, is America's: what `EST5EDT` alone has meant)
    let rest = if rest.is_empty() { ",M3.2.0,M11.1.0" } else { rest };
    let rest = rest.strip_prefix(',')?;
    let (start, rest) = posix_day(rest)?;
    let (start_time, rest) = match rest.strip_prefix('/') {
        Some(r) => posix_time(r)?,
        None => (7200, rest),
    };
    let rest = rest.strip_prefix(',')?;
    let (end, rest) = posix_day(rest)?;
    let (end_time, rest) = match rest.strip_prefix('/') {
        Some(r) => posix_time(r)?,
        None => (7200, rest),
    };
    if !rest.is_empty() {
        return None;
    }
    Some(Posix { std, dst: Some((Kind { off: dst_off, dst: true, abbr: dst_abbr }, start, start_time, end, end_time)) })
}

fn leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Days since 1970-01-01 of a rule's day in year `y`.
fn rule_day(y: i64, d: Day) -> i64 {
    let jan1 = super::time::days_from_civil(y, 1, 1);
    match d {
        Day::Zero(n) => jan1 + i64::from(n),
        Day::Julian(n) => jan1 + i64::from(n) - 1 + i64::from(leap(y) && n >= 60),
        Day::Month(m, w, dow) => {
            let first = super::time::days_from_civil(y, u32::from(m), 1);
            let first_dow = (first + 4).rem_euclid(7); // 1970-01-01 was a Thursday
            let mut day = first + (i64::from(dow) - first_dow).rem_euclid(7) + 7 * (i64::from(w) - 1);
            let len = i64::from(super::time::days_in_month(y, u32::from(m)));
            while day >= first + len {
                day -= 7; // week 5 is the last, however many there are
            }
            day
        }
    }
}

impl Posix {
    /// The kind in effect at a UTC instant (seconds since 1970).
    fn at(&self, t: i64) -> &Kind {
        let Some((dst, start, start_time, end, end_time)) = &self.dst else { return &self.std };
        let (y, _, _) = super::time::civil_from_days((t + i64::from(self.std.off)).div_euclid(86_400));
        // (the rules are in local time: the start in standard time, the end in daylight time)
        let s = rule_day(y, *start) * 86_400 + i64::from(*start_time) - i64::from(self.std.off);
        let e = rule_day(y, *end) * 86_400 + i64::from(*end_time) - i64::from(dst.off);
        let in_dst = if s < e { s <= t && t < e } else { t < e || t >= s };
        if in_dst { dst } else { &self.std }
    }
}

// ---------------------------------------------------------------- questions

impl Rules {
    /// The zone's offset east of UTC when it has had only one (pg_get_timezone_offset): None for a zone whose
    /// offset has changed, or does each year.
    pub fn fixed_offset(&self) -> Option<i32> {
        let off = self.kinds.first()?.off;
        let footer_fixed = self.footer.as_ref().is_none_or(|p| p.dst.is_none() && p.std.off == off);
        (self.kinds.iter().all(|k| k.off == off) && footer_fixed).then_some(off)
    }

    /// What the abbreviation `abbr` meant in this zone at a UTC instant (seconds since 1970): its latest use
    /// starting by then, else its first after, as pg_interpret_timezone_abbrev finds it. None if the zone's
    /// transitions never use it.
    fn abbr_kind(&self, abbr: &str, t: i64) -> Option<&Kind> {
        let cutoff = self.ats.partition_point(|a| *a <= t);
        let kind = |i: usize| self.kinds.get(usize::from(*self.idx.get(i)?));
        (0..cutoff)
            .rev()
            .filter_map(kind)
            .find(|k| k.abbr == abbr)
            .or_else(|| (cutoff..self.ats.len()).filter_map(kind).find(|k| k.abbr == abbr))
    }

    /// The kind of time a zone keeps at a UTC instant (seconds since 1970).
    pub fn at(&self, t: i64) -> &Kind {
        if let Some((zone, abbr)) = &self.abbr_of {
            return zone.abbr_kind(abbr, t).unwrap_or_else(|| zone.at(t));
        }
        match self.ats.last() {
            // (before the first transition: the first kind, as the tz library does)
            _ if self.ats.first().is_none_or(|first| t < *first) => match (&self.footer, self.ats.is_empty()) {
                (Some(p), true) => p.at(t),
                _ => &self.kinds[0],
            },
            Some(last) if t >= *last => match &self.footer {
                Some(p) => p.at(t),
                None => &self.kinds[usize::from(*self.idx.last().unwrap_or(&0))],
            },
            _ => {
                let i = self.ats.partition_point(|a| *a <= t);
                &self.kinds[usize::from(self.idx[i - 1])]
            }
        }
    }

    /// The UTC instant (seconds since 1970) a wall clock reading (seconds since 1970, as if it were UTC) is in
    /// this zone. A reading a daylight saving change skips is taken in the zone's time before the change,
    /// and one it repeats in its time after, as Postgres does.
    pub fn utc(&self, local: i64) -> i64 {
        // a dynamic abbreviation: what it meant at the moment the zone itself reads the clock as
        // (DetermineTimeZoneAbbrevOffset)
        if let Some((zone, abbr)) = &self.abbr_of {
            let t = zone.utc(local);
            return zone.abbr_kind(abbr, t).map_or(t, |k| local - i64::from(k.off));
        }
        const WINDOW: i64 = 16 * 3600 + 1;
        // the offsets in effect at any instant that could read `local`: those just before and after it
        let mut offs = vec![self.at(local - WINDOW).off, self.at(local + WINDOW).off];
        let i = self.ats.partition_point(|a| *a <= local - WINDOW);
        offs.extend(self.ats[i..].iter().take_while(|a| **a <= local + WINDOW).map(|a| self.at(*a).off));
        let first = self.ats.first().copied().unwrap_or(i64::MAX);
        if local + WINDOW >= first && self.footer.is_some() {
            // (past the table, the footer's own changes)
            for step in (-WINDOW..=WINDOW).step_by(3600) {
                offs.push(self.at(local + step).off);
            }
        }
        offs.sort_unstable();
        offs.dedup();
        let valid: Vec<i64> =
            offs.iter().map(|o| local - i64::from(*o)).filter(|u| i64::from(self.at(*u).off) == local - *u).collect();
        match valid.iter().max() {
            Some(u) => *u,
            None => local - i64::from(offs[0]), // skipped: the time before the change
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_rules_read() {
        let p = posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        assert_eq!((p.std.off, p.std.abbr.as_str()), (-18000, "EST"));
        let (dst, ..) = p.dst.clone().unwrap();
        assert_eq!((dst.off, dst.abbr.as_str()), (-14400, "EDT"));
        let p = posix("<-03>3<-02>,M3.5.0/-2,M10.5.0/-1").unwrap();
        assert_eq!(p.std.off, -10800);
        let (dst, _, st, _, et) = p.dst.unwrap();
        assert_eq!((dst.off, st, et), (-7200, -7200, -3600));
        assert!(posix("JST-9").unwrap().dst.is_none());
    }

    #[test]
    fn new_york_changes_when_it_should() {
        let Some(z) = find("america/new_york") else { return }; // (no tz database here)
        assert_eq!(z.name, "America/New_York");
        // 2024-03-10 07:00 UTC is the spring-forward moment
        let t = 1_710_054_000;
        assert_eq!((z.at(t - 1).off, z.at(t).off), (-18000, -14400));
        // local 02:30 on that day does not exist: taken as standard time, 07:30 UTC
        assert_eq!(z.utc(1_710_037_800), 1_710_055_800);
        // 2024-11-03 01:30 happens twice: the second, standard time, 06:30 UTC
        assert_eq!(z.utc(1_730_597_400), 1_730_615_400);
        // after the file's last transition the footer still changes it
        assert_eq!(z.at(4_102_444_800 + 180 * 86_400).abbr, "EDT"); // 2100-06-29
    }
}
