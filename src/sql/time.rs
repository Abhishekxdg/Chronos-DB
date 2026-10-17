//! Dates, timestamps and intervals, as Postgres writes and reads them. Dates are days since
//! 1970-01-01; timestamps are microseconds since 1970-01-01 00:00 (UTC for timestamptz);
//! intervals keep months, days and microseconds apart, since a month has no fixed length. The
//! calendar is the proleptic Gregorian one.
//!
//! A session's time zone (`SET TIME ZONE`, see `Zone`) is a fixed offset from UTC, or a named zone
//! read from the system's tz database with its daylight saving time (`tz.rs`): timestamptz is shown
//! in it, and read in it when the text has no zone of its own, and a date or plain timestamp becomes
//! a timestamptz (and back) in it. Without a tz database only the zones of `FIXED` and fixed offsets
//! are known.

pub const DAY: i64 = 86_400_000_000;
/// `infinity` and `-infinity`, as the largest and smallest date and timestamp (as in Postgres).
pub const DATE_INF: i32 = i32::MAX;
pub const DATE_NEG_INF: i32 = i32::MIN;
pub const TS_INF: i64 = i64::MAX;
pub const TS_NEG_INF: i64 = i64::MIN;

/// Whether a timestamp is `infinity` or `-infinity`.
pub fn infinite(us: i64) -> bool {
    us == TS_INF || us == TS_NEG_INF
}

/// Whether a date is `infinity` or `-infinity`.
pub fn infinite_date(d: i32) -> bool {
    d == DATE_INF || d == DATE_NEG_INF
}

/// 4714-11-24 00:00:00 BC, the earliest timestamp, in microseconds since 1970.
pub const MIN_TIMESTAMP: i64 = -210_866_803_200_000_000;
pub const HOUR: i64 = 3_600_000_000;
pub const MINUTE: i64 = 60_000_000;
pub const SECOND: i64 = 1_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    /// For ordering and equality, as Postgres does: a month is 30 days.
    pub fn total(&self) -> i128 {
        (self.months as i128 * 30 + self.days as i128) * DAY as i128 + self.micros as i128
    }
}

/// Days since 1970-01-01 of a calendar date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The calendar date of a day number.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

pub(super) fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

thread_local! {
    /// The time now() gives while set (µs since 1970): a simulation world's base moment.
    pub(crate) static FROZEN: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
    /// When the running statement's transaction began (its own start, outside BEGIN): what now(),
    /// current_timestamp and current_date give, so they agree all through a transaction, as in Postgres
    pub(crate) static XACT: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
    /// When the running statement began: statement_timestamp()
    pub(crate) static STMT: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// A session's time zone: a name, as SHOW TimeZone gives it, and its offset east of UTC (for a zone
/// with `rules`, its offset when it was named).
#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    pub name: String,
    pub secs: i32,
    /// the tz database's rules, for a zone that changes its offset (daylight saving time)
    pub rules: Option<std::sync::Arc<super::tz::Rules>>,
}

impl Zone {
    fn fixed(name: impl Into<String>, secs: i32) -> Zone {
        Zone { name: name.into(), secs, rules: None }
    }

    /// The offset east of UTC, in µs, at a UTC instant (µs since 1970).
    pub fn offset_at(&self, us: i64) -> i64 {
        match &self.rules {
            Some(r) => i64::from(r.at(us.div_euclid(SECOND)).off) * SECOND,
            None => i64::from(self.secs) * SECOND,
        }
    }

    /// The offset, in µs, that turns a wall clock reading (µs since 1970, as if UTC) into a UTC instant:
    /// `utc = local - offset_for_local(local)`.
    pub fn offset_for_local(&self, local: i64) -> i64 {
        match &self.rules {
            Some(r) => local - (r.utc(local.div_euclid(SECOND)).saturating_mul(SECOND) + local.rem_euclid(SECOND)),
            None => i64::from(self.secs) * SECOND,
        }
    }

    /// The abbreviation in use at a UTC instant (`EST`, `CEST`, `+0530`).
    pub fn abbr_at(&self, us: i64) -> String {
        match &self.rules {
            Some(r) => r.at(us.div_euclid(SECOND)).abbr.clone(),
            None if self.secs == 0 => "UTC".into(),
            None => self.name.rsplit('/').next().map_or_else(String::new, |_| numeric_abbr(self.secs)),
        }
    }
}

/// `+05`, `-0330`: how a zone with no abbreviation of its own is shown by to_char's TZ.
pub fn numeric_abbr(secs: i32) -> String {
    let (sign, secs) = (if secs < 0 { '-' } else { '+' }, secs.abs());
    match (secs % 3600 / 60, secs % 60) {
        (0, 0) => format!("{sign}{:02}", secs / 3600),
        (m, 0) => format!("{sign}{:02}{m:02}", secs / 3600),
        (m, s) => format!("{sign}{:02}{m:02}{s:02}", secs / 3600),
    }
}

/// IANA zones with one offset all year, now (an old date gets that offset too, where Postgres's
/// zone data would give the one of its time): (name, seconds east of UTC).
const FIXED: &[(&str, i32)] = &[
    ("Africa/Accra", 0),
    ("Africa/Addis_Ababa", 3 * 3600),
    ("Africa/Algiers", 3600),
    ("Africa/Johannesburg", 2 * 3600),
    ("Africa/Lagos", 3600),
    ("Africa/Nairobi", 3 * 3600),
    ("America/Argentina/Buenos_Aires", -3 * 3600),
    ("America/Bogota", -5 * 3600),
    ("America/Buenos_Aires", -3 * 3600),
    ("America/Caracas", -4 * 3600),
    ("America/Lima", -5 * 3600),
    ("America/Montevideo", -3 * 3600),
    ("America/Panama", -5 * 3600),
    ("America/Phoenix", -7 * 3600),
    ("America/Puerto_Rico", -4 * 3600),
    ("America/Sao_Paulo", -3 * 3600),
    ("Asia/Almaty", 5 * 3600),
    ("Asia/Baghdad", 3 * 3600),
    ("Asia/Bahrain", 3 * 3600),
    ("Asia/Bangkok", 7 * 3600),
    ("Asia/Calcutta", 5 * 3600 + 1800),
    ("Asia/Colombo", 5 * 3600 + 1800),
    ("Asia/Dhaka", 6 * 3600),
    ("Asia/Dubai", 4 * 3600),
    ("Asia/Ho_Chi_Minh", 7 * 3600),
    ("Asia/Hong_Kong", 8 * 3600),
    ("Asia/Jakarta", 7 * 3600),
    ("Asia/Kabul", 4 * 3600 + 1800),
    ("Asia/Karachi", 5 * 3600),
    ("Asia/Kathmandu", 5 * 3600 + 2700),
    ("Asia/Katmandu", 5 * 3600 + 2700),
    ("Asia/Kolkata", 5 * 3600 + 1800),
    ("Asia/Kuala_Lumpur", 8 * 3600),
    ("Asia/Kuwait", 3 * 3600),
    ("Asia/Manila", 8 * 3600),
    ("Asia/Muscat", 4 * 3600),
    ("Asia/Qatar", 3 * 3600),
    ("Asia/Riyadh", 3 * 3600),
    ("Asia/Saigon", 7 * 3600),
    ("Asia/Seoul", 9 * 3600),
    ("Asia/Shanghai", 8 * 3600),
    ("Asia/Singapore", 8 * 3600),
    ("Asia/Taipei", 8 * 3600),
    ("Asia/Tashkent", 5 * 3600),
    ("Asia/Tokyo", 9 * 3600),
    ("Australia/Brisbane", 10 * 3600),
    ("Australia/Darwin", 9 * 3600 + 1800),
    ("Australia/Perth", 8 * 3600),
    ("Europe/Istanbul", 3 * 3600),
    ("Europe/Minsk", 3 * 3600),
    ("Europe/Moscow", 3 * 3600),
    ("Pacific/Honolulu", -10 * 3600),
    ("Pacific/Port_Moresby", 10 * 3600),
];

/// The zone `SET TIME ZONE 'name'` names, as check_timezone reads it: UTC and its aliases, a zone of the tz
/// database or of `FIXED`, a number of hours east of UTC (`-1.5`), an offset written the POSIX way, where
/// `UTC+5` and `+05:00` are five hours *west* of UTC, or a POSIX rule (`CST7CDT,M4.1.0,M10.5.0`). Err: the
/// message for 22023.
pub fn zone_named(name: &str) -> Result<Zone, String> {
    let n = name.trim();
    let lower = n.to_ascii_lowercase();
    if ["utc", "etc/utc", "gmt", "etc/gmt", "z", "zulu", "uct", "universal", "etc/universal", "gmt0", "etc/gmt0"]
        .contains(&lower.as_str())
    {
        return Ok(Zone::fixed(if lower == "utc" { "UTC".into() } else { n.to_string() }, 0));
    }
    // the tz database's own rules, daylight saving time and history included
    if let Some(rules) = super::tz::find(n) {
        let secs = rules.at(now().div_euclid(SECOND)).off;
        return Ok(Zone { name: rules.name.clone(), secs, rules: Some(rules) });
    }
    if let Some((z, secs)) = FIXED.iter().find(|(z, _)| z.eq_ignore_ascii_case(n)) {
        return Ok(Zone::fixed(*z, *secs));
    }
    // a number alone is hours east of UTC, the SQL way (`-1.5`, `+5`), as check_timezone reads one
    if let Ok(h) = n.parse::<f64>()
        && h.is_finite()
        && !n.contains(['e', 'E'])
    {
        return zone_offset((h * 3600.0).round() as i64);
    }
    // POSIX: Etc/GMT+5, UTC+5, GMT-3:30, +05:30, 04:30 are west for + (or no sign), east for -
    let posix = ["etc/gmt", "utc", "gmt", ""].iter().find_map(|p| {
        let rest = lower.strip_prefix(p)?;
        (rest.starts_with(['+', '-'])
            || (p.is_empty() && rest.contains(':') && rest.starts_with(|c: char| c.is_ascii_digit())))
        .then_some(rest)
    });
    if let Some(r) = posix {
        let west = !r.starts_with('-');
        let body = r.strip_prefix(['+', '-']).unwrap_or(r);
        let (h, m) = body.split_once(':').unwrap_or((body, "0"));
        if let (Ok(h), Ok(m)) = (h.parse::<i32>(), m.parse::<i32>())
            && (0..=15).contains(&h)
            && (0..60).contains(&m)
        {
            let east = h * 3600 + m * 60;
            return Ok(Zone::fixed(n, if west { -east } else { east }));
        }
    }
    // a POSIX rule, as Postgres takes one no zone file names: CST7CDT,M4.1.0,M10.5.0
    if let Some(rules) = super::tz::from_posix(n) {
        let secs = rules.at(now().div_euclid(SECOND)).off;
        return Ok(Zone { name: n.to_string(), secs, rules: Some(rules) });
    }
    if super::tz::all_names().is_empty() {
        return Err(format!(
            "time zone \"{n}\" is not supported: no time zone database was found (install tzdata, or set TZDIR), so \
             Chronos knows only zones with one offset all year (such as Asia/Kolkata) and fixed offsets; set one's \
             offset, as in SET TIME ZONE INTERVAL '-05:00' HOUR TO MINUTE"
        ));
    }
    Err(format!("time zone \"{n}\" not recognized"))
}

/// A zone `secs` east of UTC, named as Postgres shows a numeric `SET TIME ZONE` (`<+05:30>-05:30`).
pub fn zone_offset(secs: i64) -> Result<Zone, String> {
    // checked wide, before narrowing: a huge offset mustn't wrap into a small one
    let Some(secs) = i32::try_from(secs).ok().filter(|s| s.abs() <= 15 * 3600 + 59 * 60) else {
        return Err(format!("time zone offset of {secs} seconds is out of range (at most 15:59 from UTC)"));
    };
    if secs == 0 {
        return Ok(Zone::fixed("UTC", 0));
    }
    // (its abbreviation is the offset in the brackets, `-01:30`, as pg_tzset_offset makes it)
    let name = format!("<{}>{}", format_offset(secs), format_offset(-secs));
    let rules = super::tz::from_posix(&name);
    Ok(Zone { name, secs, rules })
}

/// `us + i` for a timestamptz: its months and days are the session zone's calendar ones, so across a daylight
/// saving change a day is not 24 hours; only the time part is exact.
pub fn add_interval_tz(us: i64, i: Interval) -> Option<i64> {
    if i.months == 0 && i.days == 0 {
        return add_interval(us, i);
    }
    let wall = us.checked_add(offset_at(us))?;
    let w = add_interval(wall, Interval { micros: 0, ..i })?;
    w.checked_sub(offset_for_local(w))?.checked_add(i.micros)
}

impl Zone {
    /// `us + i` for a timestamptz, with the months and days counted on this zone's calendar (see `add_interval_tz`).
    pub fn add_interval(&self, us: i64, i: Interval) -> Option<i64> {
        if i.months == 0 && i.days == 0 {
            return add_interval(us, i);
        }
        let wall = us.checked_add(self.offset_at(us))?;
        let w = add_interval(wall, Interval { micros: 0, ..i })?;
        w.checked_sub(self.offset_for_local(w))?.checked_add(i.micros)
    }
}

/// What pg_timezone_names lists: each zone's (name, abbreviation, offset in seconds, is it daylight saving time)
/// right now. Without a tz database, UTC and the zones of `FIXED`.
pub fn zone_table() -> Vec<(String, String, i32, bool)> {
    let at = now().div_euclid(SECOND);
    let names = super::tz::all_names();
    if names.is_empty() {
        let mut out = vec![("UTC".to_string(), "UTC".to_string(), 0, false)];
        out.extend(FIXED.iter().map(|(n, o)| (n.to_string(), numeric_abbr(*o), *o, false)));
        return out;
    }
    names
        .into_iter()
        .filter_map(|n| {
            let r = super::tz::find(&n)?;
            let k = r.at(at);
            Some((r.name.clone(), k.abbr.clone(), k.off, k.dst))
        })
        .collect()
}

/// The zone an abbreviation names (`AT TIME ZONE 'PST'`): its offset all year round.
/// A dynamic one (`MSK`) has the offset it had at each moment in its zone.
pub fn abbrev_zone(name: &str) -> Option<Zone> {
    if let Some(rules) = super::tz::dynamic_abbrev(name) {
        let secs = rules.at(now().div_euclid(SECOND)).off;
        return Some(Zone { name: rules.name.clone(), secs, rules: Some(rules) });
    }
    super::tz::abbrev(name).map(|(secs, _)| Zone::fixed(name.trim().to_ascii_uppercase(), secs))
}

/// `+05:30`, `-03`, `+00`: an offset as Postgres writes it after a timestamptz.
pub fn format_offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let (h, m, s) = (secs.abs() / 3600, secs.abs() % 3600 / 60, secs.abs() % 60);
    match (m, s) {
        (0, 0) => format!("{sign}{h:02}"),
        (_, 0) => format!("{sign}{h:02}:{m:02}"),
        _ => format!("{sign}{h:02}:{m:02}:{s:02}"),
    }
}

thread_local! {
    /// The running statement's session's zone offset, in seconds east of UTC (see `Zone`).
    pub(crate) static ZONE: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    /// That zone's rules, when it has daylight saving time.
    static RULES: std::cell::RefCell<Option<std::sync::Arc<super::tz::Rules>>> = const { std::cell::RefCell::new(None) };
}

/// The zone the running statement's session has (None: UTC).
pub fn set_zone(z: Option<&Zone>) {
    ZONE.set(z.map_or(0, |z| z.secs));
    RULES.replace(z.and_then(|z| z.rules.clone()));
}

/// The running statement's zone, to hand to another thread (`restore_zone` there).
pub fn zone_state() -> (i32, Option<std::sync::Arc<super::tz::Rules>>, IntervalStyle) {
    (ZONE.get(), RULES.with_borrow(Clone::clone), INTERVAL_STYLE.get())
}

pub fn restore_zone((secs, rules, style): (i32, Option<std::sync::Arc<super::tz::Rules>>, IntervalStyle)) {
    ZONE.set(secs);
    RULES.replace(rules);
    INTERVAL_STYLE.set(style);
}

/// The session zone's offset at a UTC instant (µs since 1970), in microseconds: local time is UTC plus this.
pub fn offset_at(us: i64) -> i64 {
    RULES.with_borrow(|r| match r {
        Some(r) => i64::from(r.at(us.div_euclid(SECOND)).off) * SECOND,
        None => ZONE.get() as i64 * SECOND,
    })
}

/// The offset that turns a wall clock reading in the session's zone (µs since 1970, as if UTC) into the
/// UTC instant it is: `utc = local - offset_for_local(local)`.
pub fn offset_for_local(local: i64) -> i64 {
    RULES.with_borrow(|r| match r {
        Some(r) => local - (r.utc(local.div_euclid(SECOND)).saturating_mul(SECOND) + local.rem_euclid(SECOND)),
        None => ZONE.get() as i64 * SECOND,
    })
}

/// The session zone's abbreviation at a UTC instant (`EST`), if it has a name for it.
pub fn session_abbr_at(us: i64) -> Option<String> {
    RULES.with_borrow(|r| r.as_ref().map(|r| r.at(us.div_euclid(SECOND)).abbr.clone()))
}

/// A zone without rules' abbreviation: `UTC` for the session's default zone, else its offset (`+05:30`).
pub fn fallback_abbr(off: i32) -> String {
    if ZONE.get() == 0 && off == 0 { "UTC".into() } else { numeric_abbr(off) }
}

/// The session zone's offset now, in microseconds.
pub fn offset() -> i64 {
    offset_at(now())
}

/// A timestamptz as Postgres shows it in the session's zone: `2026-09-29 10:30:00+05:30`.
pub fn format_timestamptz(us: i64) -> String {
    if infinite(us) {
        return format_timestamp(us, true, ' ');
    }
    let off = offset_at(us);
    if DATE_STYLE.get().0 != DateStyle::Iso {
        let abbr = session_abbr_at(us).unwrap_or_else(|| fallback_abbr((off / SECOND) as i32));
        return stamp(us.saturating_add(off), &abbr, ' ');
    }
    stamp(us.saturating_add(off), &format_offset((off / SECOND) as i32), ' ')
}

/// Text as a timestamptz: in its own zone if it names one, else in the session's.
pub fn parse_timestamptz(s: &str) -> Result<i64, Bad> {
    decode::timestamp(s, true)
}

/// Today's date in the session's zone (current_date).
pub fn today() -> i32 {
    (now() + offset()).div_euclid(DAY) as i32
}

/// When the server started (its first database was opened), µs since 1970: pg_postmaster_start_time().
pub fn started() -> i64 {
    static AT: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *AT.get_or_init(real_now)
}

/// The wall clock, µs since 1970.
pub fn real_now() -> i64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    d.as_micros() as i64
}

/// now(), current_timestamp, current_date: when the transaction began (a simulation's base moment
/// while one runs; the wall clock where no statement is running).
pub fn now() -> i64 {
    FROZEN.get().or_else(|| XACT.get()).unwrap_or_else(real_now)
}

/// statement_timestamp(): when the running statement began.
pub fn statement_now() -> i64 {
    FROZEN.get().or_else(|| STMT.get()).unwrap_or_else(real_now)
}

/// clock_timestamp(): the wall clock, different at every call (a simulation's base moment while one runs).
pub fn clock_now() -> i64 {
    FROZEN.get().unwrap_or_else(real_now)
}

/// The latest year a date may have, as in Postgres (and the earliest, negated): its days
/// since 1970 fit an i32. Their microseconds don't always fit an i64 (a timestamp), which
/// reaches only to about 294,000 AD: see `date_micros`.
const MAX_YEAR: i64 = 5_874_897;

/// A date's midnight as microseconds since 1970 (a timestamp), if it has one.
pub fn date_micros(d: i32) -> Option<i64> {
    (d as i64).checked_mul(DAY)
}

/// Why text isn't a date or timestamp, as Postgres tells them apart: it isn't one (22007), it
/// is one out of range (22008), a field of it is out of range, as February 30 (22008), or its
/// zone is more than 15:59 from UTC (22009).
#[derive(Debug, PartialEq)]
pub enum Bad {
    Syntax,
    Range,
    Field,
    Zone,
    /// a month or day out of range, which DateStyle's order may explain (the error has a hint)
    MdField,
    /// a zone's name no zone has (22023), as the text spelled it, lower-cased
    NoZone(String),
    /// an interval whose months don't fit (22008 "interval out of range")
    IntervalOut,
}

mod tochar;
pub use tochar::{CharErr, to_char, to_char_interval};

mod decode;
pub(crate) use decode::{
    IM_DAY, IM_HOUR, IM_MINUTE, IM_MONTH, IM_SECOND, IM_YEAR, INTERVAL_FULL_RANGE, SaneZoneErr, sane_numeric_zone,
};

/// A time of day's older reader, for text the Postgres one (`decode`) calls a syntax error: what it took is still
/// taken.
fn or_loose<T>(new: Result<T, Bad>, old: impl FnOnce() -> Result<T, Bad>) -> Result<T, Bad> {
    match new {
        Err(Bad::Syntax) => old().map_err(|_| Bad::Syntax),
        r => r,
    }
}

/// Text as a date, as date_in reads it (`1999-01-08`, `January 8, 1999`, `J2451187`, `epoch`, ...: see `decode`).
pub fn parse_date(s: &str) -> Result<i32, Bad> {
    decode::date(s)
}

/// Text as microseconds since 1970, as timestamp_in reads it (see `decode`). With `tz` (timestamptz_in) a zone it
/// names is applied, else the session's; without, a zone is ignored, as Postgres does for plain timestamps.
pub fn parse_timestamp(s: &str, tz: bool) -> Result<i64, Bad> {
    decode::timestamp(s, tz)
}

/// 1 when the digits after the seconds' point go past six and the seventh is 5 or more.
fn frac_digits_round(sec: &str) -> bool {
    sec.split_once('.').is_some_and(|(_, f)| f.as_bytes().get(6).is_some_and(|d| *d >= b'5'))
}

/// `HH:MM[:SS[.ffffff]]` and what follows it.
fn time_part(s: &str) -> Result<(i64, &str), Bad> {
    time_part_of(s, 24)
}

/// `time_part` with as many hours as `hours` allows: an interval's are not a time of day's.
fn time_part_of(s: &str, hours: i64) -> Result<(i64, &str), Bad> {
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == ':' || c == '.')).unwrap_or(s.len());
    let (t, rest) = s.split_at(end);
    let mut it = t.split(':');
    let num = |n: Option<&str>| n.and_then(|n| n.parse::<i64>().ok()).ok_or(Bad::Syntax);
    let h = num(it.next())?;
    let m = num(it.next())?;
    let sec = it.next().unwrap_or("0");
    if it.next().is_some() {
        return Err(Bad::Syntax);
    }
    let (whole, frac) = sec.split_once('.').unwrap_or((sec, ""));
    let secs = num(Some(whole))?;
    let frac: String = frac.chars().chain(std::iter::repeat('0')).take(6).collect();
    // (a seventh digit rounds the sixth: 23:59:59.9999999 is 24:00:00)
    let round = i64::from(frac_digits_round(sec));
    let micros = num(Some(&frac))? + round;
    if h > hours || m > 59 || secs > 60 {
        return Err(Bad::Field);
    }
    Ok((h * HOUR + m * MINUTE + secs * SECOND + micros, rest))
}

pub fn format_date(days: i32) -> String {
    match days {
        DATE_INF => return "infinity".into(),
        DATE_NEG_INF => return "-infinity".into(),
        _ => {}
    }
    let (y, m, d) = civil_from_days(days as i64);
    // years before the common era count back from 1 BC (the calendar's year 0)
    let (bc, y) = if y <= 0 { (" BC", 1 - y) } else { ("", y) };
    let (style, order) = DATE_STYLE.get();
    let day_first = order == DateOrder::Dmy;
    match style {
        DateStyle::Iso => format!("{y:04}-{m:02}-{d:02}{bc}"),
        DateStyle::German => format!("{d:02}.{m:02}.{y:04}{bc}"),
        // (YMD only says how dates are read: SQL and Postgres write month first unless DMY, as Postgres does)
        DateStyle::Postgres if day_first => format!("{d:02}-{m:02}-{y:04}{bc}"),
        DateStyle::Postgres => format!("{m:02}-{d:02}-{y:04}{bc}"),
        DateStyle::Sql if day_first => format!("{d:02}/{m:02}/{y:04}{bc}"),
        DateStyle::Sql => format!("{m:02}/{d:02}/{y:04}{bc}"),
    }
}

/// A date and time of day, `zone` after the time and BC after that.
fn stamp(us: i64, zone: &str, sep: char) -> String {
    let (days, t) = (us.div_euclid(DAY), us.rem_euclid(DAY));
    let (style, order) = DATE_STYLE.get();
    if sep == ' ' && style == DateStyle::Postgres {
        const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let (y, m, d) = civil_from_days(days);
        let (bc, y) = if y <= 0 { (" BC", 1 - y) } else { ("", y) };
        let dow = (days + 4).rem_euclid(7) as usize;
        let (mon, dd) = (MONTHS[m as usize - 1], format!("{d:02}"));
        let md = if order == DateOrder::Dmy { format!("{dd} {mon}") } else { format!("{mon} {dd}") };
        let tz = if zone.is_empty() { String::new() } else { format!(" {zone}") };
        return format!("{} {md} {} {y:04}{tz}{bc}", DAYS[dow], format_time(t));
    }
    let date = format_date(days as i32);
    let (date, bc) = match date.strip_suffix(" BC") {
        Some(d) => (d, " BC"),
        None => (date.as_str(), ""),
    };
    let zone =
        if style != DateStyle::Iso && !zone.is_empty() && sep == ' ' { format!(" {zone}") } else { zone.to_string() };
    format!("{date}{sep}{}{zone}{bc}", format_time(t))
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]`, with `+00` for timestamptz; `sep` is ' ' (Postgres text)
/// or 'T' (JSON).
pub fn format_timestamp(us: i64, tz: bool, sep: char) -> String {
    match us {
        TS_INF => return "infinity".into(),
        TS_NEG_INF => return "-infinity".into(),
        _ => {}
    }
    let zone = match (tz, sep) {
        (false, _) => "",
        (true, 'T') => "+00:00",
        (true, _) => "+00",
    };
    stamp(us, zone, sep)
}

/// `HH:MM[:SS[.ffffff]]` as microseconds since midnight; `24:00:00` is allowed, as in Postgres.
pub fn parse_time(s: &str) -> Option<i64> {
    parse_time_checked(s).ok()
}

/// `parse_time`, with what is wrong: a field out of range is not a syntax error.
pub fn parse_time_checked(s: &str) -> Result<i64, Bad> {
    or_loose(decode::time(s).map(|(us, _)| us), || parse_time_loose(s))
}

/// time with time zone's text: (µs since midnight, offset east of UTC in seconds), as timetz_in reads it.
pub fn parse_timetz(s: &str) -> Result<(i64, i32), Bad> {
    decode::time(s)
}

fn parse_time_loose(s: &str) -> Result<i64, Bad> {
    let strict = |t: &str| -> Result<i64, Bad> {
        let (us, rest) = time_part(t.trim())?;
        if !rest.trim().is_empty() {
            return Err(Bad::Syntax);
        }
        if us > DAY {
            return Err(Bad::Field);
        }
        Ok(us)
    };
    match strict(s) {
        Err(Bad::Syntax) => {}
        r => return r,
    }
    // a zone after it is decoration (`02:03 PST`), as is a date before it (`2003-03-07 15:36:39 America/New_York`);
    // AM and PM move the hour
    let words: Vec<&str> = s.split_whitespace().collect();
    let at =
        words.iter().position(|w| w.starts_with(|c: char| c.is_ascii_digit()) && w.contains(':')).ok_or(Bad::Syntax)?;
    let mut t = words[at].to_string();
    let rest = &words[at + 1..];
    let (zone_words, ampm) = match rest.first() {
        Some(w) if w.eq_ignore_ascii_case("am") || w.eq_ignore_ascii_case("pm") => {
            (&rest[1..], Some(w.eq_ignore_ascii_case("pm")))
        }
        _ => (rest, None),
    };
    let known = |w: &str| {
        w.eq_ignore_ascii_case("z")
            || w.starts_with(['+', '-'])
            || super::tz::abbrev(w).is_some()
            || (at > 0 && super::tz::find(w).is_some())
    };
    if !zone_words.iter().all(|w| known(w)) || zone_words.len() > 1 {
        return Err(Bad::Syntax);
    }
    if let Some(pm) = ampm {
        let (h, rest) = t.split_once(':').ok_or(Bad::Syntax)?;
        let h: i64 = h.parse().map_err(|_| Bad::Syntax)?;
        if !(1..=12).contains(&h) {
            return Err(Bad::Field);
        }
        let h = match (h, pm) {
            (12, false) => 0,
            (h, true) if h < 12 => h + 12,
            (h, _) => h,
        };
        t = format!("{h:02}:{rest}");
    }
    strict(&t)
}

/// A time of day, `HH:MM:SS[.ffffff]`.
pub fn format_time(t: i64) -> String {
    let (h, m, s, us) = (t / HOUR, t % HOUR / MINUTE, t % MINUTE / SECOND, t % SECOND);
    let mut out = format!("{h:02}:{m:02}:{s:02}");
    if us != 0 {
        out += format!(".{us:06}").trim_end_matches('0');
    }
    out
}

/// Postgres's style: `1 year 2 mons 3 days 04:05:06`.
pub fn format_interval(i: Interval) -> String {
    match INTERVAL_STYLE.get() {
        IntervalStyle::Postgres => {}
        IntervalStyle::Iso8601 => return interval_iso8601(i),
        IntervalStyle::SqlStandard => return interval_sql_standard(i),
        IntervalStyle::PostgresVerbose => return interval_verbose(i),
    }
    // EncodeInterval's postgres style: a field after a negative one says its sign (`-1 days +02:03:00`), so the text
    // reads back the same under the SQL standard's sign rules
    let (mut out, mut is_zero, mut is_before) = (String::new(), true, false);
    for (n, unit) in [(i64::from(i.months / 12), "year"), (i64::from(i.months % 12), "mon"), (i64::from(i.days), "day")]
    {
        if n == 0 {
            continue;
        }
        out += &format!(
            "{}{}{n} {unit}{}",
            if is_zero { "" } else { " " },
            if is_before && n > 0 { "+" } else { "" },
            if n == 1 { "" } else { "s" }
        );
        is_before = n < 0;
        is_zero = false;
    }
    if is_zero || i.micros != 0 {
        // (by its size, as the smallest time has no positive of its own)
        let sign = if i.micros < 0 {
            "-"
        } else if is_before {
            "+"
        } else {
            ""
        };
        let u = i.micros.unsigned_abs();
        let (h, m, sec, frac) = (u / 3_600_000_000, u / 60_000_000 % 60, u / 1_000_000 % 60, u % 1_000_000);
        let frac = if frac == 0 { String::new() } else { format!(".{frac:06}").trim_end_matches('0').to_string() };
        out += &format!("{}{sign}{h:02}:{m:02}:{sec:02}{frac}", if is_zero { "" } else { " " });
    }
    out
}

/// How a session's intervals are written (`SET IntervalStyle`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum IntervalStyle {
    #[default]
    Postgres,
    PostgresVerbose,
    SqlStandard,
    Iso8601,
}

impl IntervalStyle {
    /// The style a setting's value names (as `SET` checked it), Postgres's when it names none.
    pub fn named(name: &str) -> Self {
        match name.trim().to_lowercase().as_str() {
            "iso_8601" => Self::Iso8601,
            "sql_standard" => Self::SqlStandard,
            "postgres_verbose" => Self::PostgresVerbose,
            _ => Self::Postgres,
        }
    }
}

/// An interval's fields as Postgres's `EncodeInterval` takes them: whole years and months out of
/// the months, and the time as hours (not wrapped at a day), minutes, seconds and microseconds,
/// each carrying the time's sign.
fn interval_fields(i: Interval) -> (i64, i64, i64, i64, i64, i64, i64) {
    let t = i.micros;
    let (year, mon) = ((i.months / 12) as i64, (i.months % 12) as i64);
    let (hour, min, sec, frac) = (t / HOUR, t % HOUR / MINUTE, t % MINUTE / SECOND, t % SECOND);
    (year, mon, i.days as i64, hour, min, sec, frac)
}

/// `sec` and its fraction as `5`, `5.5` or `05.000001`, without the fraction's trailing zeros.
fn seconds_text(sec: i64, frac: i64, zero_fill: bool) -> String {
    let (sec, frac) = (sec.abs(), frac.abs());
    let whole = if zero_fill { format!("{sec:02}") } else { sec.to_string() };
    if frac == 0 {
        return whole;
    }
    format!("{whole}.{:06}", frac).trim_end_matches('0').to_string()
}

/// `P1Y2M3DT4H5M6.5S`: ISO 8601 with designators, each field signed on its own.
fn interval_iso8601(i: Interval) -> String {
    let (year, mon, day, hour, min, sec, frac) = interval_fields(i);
    let mut out = String::from("P");
    for (n, unit) in [(year, 'Y'), (mon, 'M'), (day, 'D')] {
        if n != 0 {
            out += &format!("{n}{unit}");
        }
    }
    let time =
        [(hour, 'H'), (min, 'M')].iter().filter(|(n, _)| *n != 0).map(|(n, u)| format!("{n}{u}")).collect::<String>();
    let secs = if sec != 0 || frac != 0 {
        let sign = if sec < 0 || frac < 0 { "-" } else { "" };
        format!("{sign}{}S", seconds_text(sec, frac, false))
    } else {
        String::new()
    };
    if !time.is_empty() || !secs.is_empty() {
        out += &format!("T{time}{secs}");
    } else if out == "P" {
        out += "T0S";
    }
    out
}

/// `1-2 3 4:05:06`: the SQL standard's year-month and day-time forms, with every sign written
/// out when the interval mixes them (`+1-2 -3 +4:05:06`).
fn interval_sql_standard(i: Interval) -> String {
    let (mut year, mut mon, mut day, mut hour, mut min, mut sec, mut frac) = interval_fields(i);
    let all = [year, mon, day, hour, min, sec, frac];
    let (neg, pos) = (all.iter().any(|n| *n < 0), all.iter().any(|n| *n > 0));
    let (has_ym, has_dt) = (year != 0 || mon != 0, day != 0 || hour != 0 || min != 0 || sec != 0 || frac != 0);
    let standard = !((neg && pos) || (has_ym && has_dt));
    let mut out = String::new();
    if neg && standard {
        out.push('-');
        (year, mon, day, hour, min, sec, frac) = (-year, -mon, -day, -hour, -min, -sec, -frac);
    }
    let secs = seconds_text(sec, frac, true);
    if !neg && !pos {
        out.push('0');
    } else if !standard {
        let sign = |n: bool| if n { '-' } else { '+' };
        out += &format!(
            "{}{}-{} {}{} {}{}:{:02}:{secs}",
            sign(year < 0 || mon < 0),
            year.abs(),
            mon.abs(),
            sign(day < 0),
            day.abs(),
            sign(hour < 0 || min < 0 || sec < 0 || frac < 0),
            hour.abs(),
            min.abs()
        );
    } else if has_ym {
        out += &format!("{year}-{mon}");
    } else if day != 0 {
        out += &format!("{day} {hour}:{min:02}:{secs}");
    } else {
        out += &format!("{hour}:{min:02}:{secs}");
    }
    out
}

/// `@ 1 year 2 mons 3 days 4 hours 5 mins 6.5 secs`, with `ago` when the first field is negative.
fn interval_verbose(i: Interval) -> String {
    let (year, mon, day, hour, min, sec, frac) = interval_fields(i);
    let (mut zero, mut before) = (true, false);
    let mut out = String::from("@");
    for (n, unit) in [(year, "year"), (mon, "mon"), (day, "day"), (hour, "hour"), (min, "min")] {
        if n == 0 {
            continue;
        }
        let n = if zero {
            zero = false;
            before = n < 0;
            n.abs()
        } else if before {
            -n
        } else {
            n
        };
        out += &format!(" {n} {unit}{}", if n == 1 { "" } else { "s" });
    }
    if sec != 0 || frac != 0 {
        out.push(' ');
        if sec < 0 || (sec == 0 && frac < 0) {
            if zero {
                before = true;
            } else if !before {
                out.push('-');
            }
        } else if before {
            out.push('-');
        }
        out += &format!("{} sec{}", seconds_text(sec, frac, false), if sec.abs() == 1 && frac == 0 { "" } else { "s" });
        zero = false;
    }
    if zero {
        out += " 0";
    }
    if before {
        out += " ago";
    }
    out
}

/// DateStyle's output style and its order of month and day.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum DateStyle {
    #[default]
    Iso,
    Postgres,
    Sql,
    German,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum DateOrder {
    #[default]
    Mdy,
    Dmy,
    Ymd,
}

thread_local! {
    static DATE_STYLE: std::cell::Cell<(DateStyle, DateOrder)> = const { std::cell::Cell::new((DateStyle::Iso, DateOrder::Mdy)) };
}

/// Runs `f` with dates and timestamps written in ISO 8601, whatever the session's DateStyle: what JSON and stored
/// values hold, which are read back whatever the style then is.
pub fn iso<T>(f: impl FnOnce() -> T) -> T {
    let was = DATE_STYLE.replace((DateStyle::Iso, DateOrder::Mdy));
    let out = f();
    DATE_STYLE.set(was);
    out
}

/// The DateStyle the running statement's session writes dates and timestamps in.
pub fn set_date_style(style: DateStyle, order: DateOrder) {
    DATE_STYLE.set((style, order));
}

/// The DateStyle the running statement's session writes dates and timestamps in.
pub fn date_style() -> (DateStyle, DateOrder) {
    DATE_STYLE.get()
}

/// `iso, mdy`, `german`, `sql, dmy`: the style and order the words say, or None for a word that is neither.
pub fn parse_date_style(words: &[&str]) -> Option<(DateStyle, DateOrder)> {
    parse_date_style_from((DateStyle::Iso, DateOrder::Mdy), words)
}

/// `parse_date_style`, as SET DateStyle reads it, after `base` (the style it had): what the words leave out stays
/// as it was (`SET datestyle = 'SQL'` keeps the order), as Postgres's check_datestyle; None for a word that is
/// neither, or two styles or orders that differ.
pub fn parse_date_style_from(base: (DateStyle, DateOrder), words: &[&str]) -> Option<(DateStyle, DateOrder)> {
    let (mut style, mut order) = (None, None);
    let put = |slot: &mut Option<DateOrder>, o: DateOrder| match slot.replace(o) {
        Some(was) if was != o => None,
        _ => Some(()),
    };
    for w in words {
        let s = match *w {
            "iso" => Some(DateStyle::Iso),
            "postgres" => Some(DateStyle::Postgres),
            "sql" => Some(DateStyle::Sql),
            "german" => Some(DateStyle::German),
            _ => None,
        };
        if let Some(s) = s {
            if style.replace(s).is_some_and(|was| was != s) {
                return None;
            }
            continue;
        }
        match *w {
            "mdy" | "us" | "noneuropean" => put(&mut order, DateOrder::Mdy)?,
            "dmy" | "euro" | "european" => put(&mut order, DateOrder::Dmy)?,
            "ymd" => put(&mut order, DateOrder::Ymd)?,
            // (the reset value, ISO, MDY, for what the others leave out)
            "default" => {
                style = style.or(Some(DateStyle::Iso));
                order = order.or(Some(DateOrder::Mdy));
            }
            _ => return None,
        }
    }
    // (German is day first unless the order says otherwise)
    let order = match (order, style) {
        (Some(o), _) => o,
        (None, Some(DateStyle::German)) => DateOrder::Dmy,
        (None, _) => base.1,
    };
    Some((style.unwrap_or(base.0), order))
}

/// How Postgres spells a DateStyle: `ISO, MDY`, `Postgres, DMY`, `German, DMY`.
pub fn date_style_name(style: DateStyle, order: DateOrder) -> String {
    let s = match style {
        DateStyle::Iso => "ISO",
        DateStyle::Postgres => "Postgres",
        DateStyle::Sql => "SQL",
        DateStyle::German => "German",
    };
    let o = match order {
        DateOrder::Mdy => "MDY",
        DateOrder::Dmy => "DMY",
        DateOrder::Ymd => "YMD",
    };
    format!("{s}, {o}")
}

thread_local! {
    /// The running statement's session's `IntervalStyle` (see `IntervalStyle`).
    static INTERVAL_STYLE: std::cell::Cell<IntervalStyle> = const { std::cell::Cell::new(IntervalStyle::Postgres) };
}

/// The style the running statement's session writes intervals in.
pub fn set_interval_style(style: IntervalStyle) {
    INTERVAL_STYLE.set(style);
}

/// Runs `f` with intervals written (and read) in `style`, whatever the session's is.
pub fn with_interval_style<T>(style: IntervalStyle, f: impl FnOnce() -> T) -> T {
    let was = INTERVAL_STYLE.replace(style);
    let out = f();
    INTERVAL_STYLE.set(was);
    out
}

/// `1 day`, `2 hours 30 minutes`, `-1 week`, `1 year 2 mons`, `04:05:06`, `3 days 04:05`, `P1Y2M3DT4H5M6S`, ...
/// None past what a field holds, too.
pub fn parse_interval(s: &str) -> Option<Interval> {
    parse_interval_checked(s).ok()
}

/// `parse_interval`, with what is wrong: text that isn't an interval (`Bad::Syntax`), or one past what its months,
/// days or microseconds hold (`Bad::Range`).
pub fn parse_interval_checked(s: &str) -> Result<Interval, Bad> {
    parse_interval_fields(s, INTERVAL_FULL_RANGE, None)
}

/// interval_in with a typmod: the text read by its fields (`range`, of the `IM_` masks) and rounded to `precision`
/// digits of seconds. Errors: `Bad::Syntax`, `Bad::Range` (a field out of range), `Bad::IntervalOut`.
pub fn parse_interval_fields(s: &str, range: u32, precision: Option<u32>) -> Result<Interval, Bad> {
    decode::interval(s, range, precision).map_err(|e| match e {
        decode::IntervalBad::Syntax => Bad::Syntax,
        decode::IntervalBad::Field => Bad::Range,
        decode::IntervalBad::Range => Bad::IntervalOut,
    })
}

/// A timestamp moved by an interval: months by the calendar (Jan 31 + 1 month is Feb 28/29),
/// then days, then time.
/// None past the microseconds an i64 holds (about 292,000 years from 1970).
pub fn add_interval(us: i64, i: Interval) -> Option<i64> {
    let (day, t) = (us.div_euclid(DAY), us.rem_euclid(DAY));
    let (y, m, d) = civil_from_days(day);
    let months = y * 12 + (m as i64 - 1) + i.months as i64;
    let (ny, nm) = (months.div_euclid(12), (months.rem_euclid(12) + 1) as u32);
    let nd = d.min(days_in_month(ny, nm));
    let us = (days_from_civil(ny, nm, nd) as i128 + i.days as i128) * DAY as i128 + t as i128 + i.micros as i128;
    i64::try_from(us).ok()
}

/// The difference of two timestamps, in days and time, as Postgres gives it.
pub fn between(a: i64, b: i64) -> Interval {
    let d = a as i128 - b as i128; // two far timestamps are further apart than an i64 holds
    Interval { months: 0, days: (d / DAY as i128) as i32, micros: (d % DAY as i128) as i64 }
}

/// `date_trunc(unit, ts)`: the start of its microsecond, second, ..., year.
/// The unit is read as extract reads its (`MILLENNIUM`, `mon`, `centuries`); a century and a millennium start at
/// their year 1 (2001, and 100 BC for 55 BC), as timestamp_trunc has them.
pub fn trunc(unit: &str, us: i64) -> Option<i64> {
    let (day, t) = (us.div_euclid(DAY), us.rem_euclid(DAY));
    let (y, m, _) = civil_from_days(day);
    Some(match field_name(unit)? {
        "microseconds" => us,
        "milliseconds" => us - us.rem_euclid(1000),
        "second" => us - us.rem_euclid(SECOND),
        "minute" => us - us.rem_euclid(MINUTE),
        "hour" => us - us.rem_euclid(HOUR),
        "day" => us - t,
        "week" => (day - (day + 3).rem_euclid(7)) * DAY, // weeks start on Monday
        "month" => days_from_civil(y, m, 1) * DAY,
        "quarter" => days_from_civil(y, (m - 1) / 3 * 3 + 1, 1) * DAY,
        "year" => days_from_civil(y, 1, 1) * DAY,
        "decade" => days_from_civil(y - y.rem_euclid(10), 1, 1) * DAY,
        "century" => {
            let c = if y > 0 { (y + 99) / 100 * 100 - 99 } else { -((99 - (y - 1)) / 100) * 100 + 1 };
            days_from_civil(c, 1, 1) * DAY
        }
        "millennium" => {
            let k = if y > 0 { (y + 999) / 1000 * 1000 - 999 } else { -((999 - (y - 1)) / 1000) * 1000 + 1 };
            days_from_civil(k, 1, 1) * DAY
        }
        _ => return None,
    })
}

/// A type `extract` reads, which decides the fields it has and how some are written.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Date,
    Timestamp,
    TimestampTz,
    Time,
}

/// What a field of `extract` came to.
pub enum Field {
    Value(super::decimal::Dec),
    /// a field Postgres has, but not for this type (0A000)
    Unsupported,
    /// no such field (22023)
    Unknown,
}

/// A field's name as Postgres reads it: singular, lower case. (As its tables are searched, only the first ten
/// characters count: `timezone_hour` is `timezone_h`.)
fn field_name(field: &str) -> Option<&'static str> {
    let lower = field.trim().to_ascii_lowercase();
    let key = lower.char_indices().nth(10).map_or(lower.as_str(), |(at, _)| &lower[..at]);
    Some(match key {
        "microsecon" => "microseconds",
        "millisecon" => "milliseconds",
        "timezone_h" => "timezone_hour",
        "timezone_m" => "timezone_minute",
        "century" | "centuries" | "c" | "cent" | "cents" => "century",
        "day" | "days" | "d" => "day",
        "decade" | "decades" | "dec" | "decs" => "decade",
        "dow" => "dow",
        "doy" => "doy",
        "epoch" => "epoch",
        "hour" | "hours" | "h" | "hr" | "hrs" => "hour",
        "isodow" => "isodow",
        "isoyear" => "isoyear",
        "julian" => "julian",
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" | "usecond" | "useconds" => "microseconds",
        "millennium" | "millennia" | "millenniums" | "mil" | "mils" => "millennium",
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" | "msecond" | "mseconds" => "milliseconds",
        "minute" | "minutes" | "min" | "mins" => "minute",
        "month" | "months" | "mon" | "mons" => "month",
        "quarter" | "qtr" => "quarter",
        "second" | "seconds" | "s" | "sec" | "secs" => "second",
        "week" | "weeks" | "w" => "week",
        "year" | "years" | "y" | "yr" | "yrs" => "year",
        "timezone" => "timezone",
        "timezone_hour" => "timezone_hour",
        "timezone_minute" => "timezone_minute",
        _ => return None,
    })
}

/// The unit extract and date_trunc read `field` as (`year` for `YRS`), if they know it.
pub fn unit_name(field: &str) -> Option<&'static str> {
    field_name(field)
}

/// The year as Postgres numbers it: no year 0, 1 BC is -1.
fn year_field(y: i64) -> i64 {
    if y <= 0 { y - 1 } else { y }
}

/// `extract(field from value)` as Postgres 14 and later gives it, a numeric with the scale it
/// writes each field at. `wall` is the value's clock in microseconds (in the session's zone for a
/// timestamptz), `utc` the moment it is, `offset` the zone's seconds east of UTC (a timestamptz's).
pub fn extract_numeric(field: &str, src: Source, wall: i64, utc: i64, offset: i64) -> Field {
    use super::decimal::Dec;
    let Some(f) = field_name(field) else { return Field::Unknown };
    let int = |n: i64| Field::Value(Dec::int(n));
    let scaled = |m: i64, scale: u32| Field::Value(Dec::new(i128::from(m), scale));
    let (day, t) = (wall.div_euclid(DAY), wall.rem_euclid(DAY));
    let has_time = src != Source::Date;
    if src == Source::Time {
        return match f {
            "hour" => int(t / HOUR),
            "minute" => int(t % HOUR / MINUTE),
            "second" => scaled(t % MINUTE, 6),
            "milliseconds" => scaled(t % MINUTE, 3),
            "microseconds" => int(t % MINUTE),
            "epoch" => scaled(t, 6),
            _ => Field::Unsupported,
        };
    }
    let (y, m, d) = civil_from_days(day);
    let year = year_field(y);
    // the ISO 8601 week: the one holding this week's Thursday, which is in its year
    let isodow = (day + 3).rem_euclid(7) + 1;
    let thursday = day - (isodow - 1) + 3;
    let (ty, ..) = civil_from_days(thursday);
    match f {
        "year" => int(year),
        "month" => int(m as i64),
        "day" => int(d as i64),
        "quarter" => int(i64::from((m - 1) / 3 + 1)),
        // (counted from the astronomical year, where 1 BC is 0, as timestamp_part counts them)
        "decade" => int(if y > 0 { y / 10 } else { -((8 - (y - 1)) / 10) }),
        "century" => int(if y > 0 { (y + 99) / 100 } else { -((99 - (y - 1)) / 100) }),
        "millennium" => int(if y > 0 { (y + 999) / 1000 } else { -((999 - (y - 1)) / 1000) }),
        "dow" => int((day + 4).rem_euclid(7)),
        "isodow" => int(isodow),
        "doy" => int(day - days_from_civil(y, 1, 1) + 1),
        "isoyear" => int(year_field(ty)),
        "week" => int((thursday - days_from_civil(ty, 1, 1)) / 7 + 1),
        "julian" if !has_time => int(day + 2_440_588),
        "julian" => {
            let fraction = Dec::div_at(Dec::int(t), Dec::int(DAY), 20).unwrap_or(Dec::int(0));
            Dec::add(Dec::int(day + 2_440_588), fraction).map_or(Field::Unsupported, Field::Value)
        }
        "epoch" if !has_time => int(day * 86_400),
        "epoch" => scaled(utc, 6),
        "hour" | "minute" | "second" | "milliseconds" | "microseconds" if !has_time => Field::Unsupported,
        "hour" => int(t / HOUR),
        "minute" => int(t % HOUR / MINUTE),
        "second" => scaled(t % MINUTE, 6),
        "milliseconds" => scaled(t % MINUTE, 3),
        "microseconds" => int(t % MINUTE),
        "timezone" if src == Source::TimestampTz => int(offset),
        "timezone_hour" if src == Source::TimestampTz => int(offset / 3600),
        "timezone_minute" if src == Source::TimestampTz => int(offset % 3600 / 60),
        _ => Field::Unsupported,
    }
}

/// `extract(field from interval)`, as a numeric at Postgres's scale.
pub fn extract_interval_numeric(field: &str, i: Interval) -> Field {
    use super::decimal::Dec;
    let Some(f) = field_name(field) else { return Field::Unknown };
    let int = |n: i64| Field::Value(Dec::int(n));
    let scaled = |m: i64, scale: u32| Field::Value(Dec::new(i128::from(m), scale));
    let (years, months) = (i64::from(i.months / 12), i64::from(i.months % 12));
    match f {
        "year" => int(years),
        "month" => int(months),
        "day" => int(i64::from(i.days)),
        "decade" => int(years / 10),
        "century" => int(years / 100),
        "millennium" => int(years / 1000),
        "quarter" => int(months / 3 + 1),
        "hour" => int(i.micros / HOUR),
        "minute" => int(i.micros % HOUR / MINUTE),
        "second" => scaled(i.micros % MINUTE, 6),
        "milliseconds" => scaled(i.micros % MINUTE, 3),
        "microseconds" => int(i.micros % MINUTE),
        // a year is 365.25 days and a month 30 days here, as Postgres counts them
        "epoch" => {
            let secs = years * 31_557_600 + months * 2_592_000 + i64::from(i.days) * 86_400;
            Dec::add(Dec::int(secs).rescale(6).unwrap_or(Dec::int(0)), Dec::new(i128::from(i.micros), 6))
                .map_or(Field::Unsupported, Field::Value)
        }
        _ => Field::Unsupported,
    }
}

/// Postgres's `age(a, b)`: the difference in whole years, months and days (by the calendar,
/// borrowing from `b`'s month), then time.
pub fn age(a: i64, b: i64) -> Interval {
    if a < b {
        let i = age(b, a);
        return Interval { months: -i.months, days: -i.days, micros: -i.micros };
    }
    let split = |us: i64| {
        let (days, t) = (us.div_euclid(DAY), us.rem_euclid(DAY));
        let (y, m, d) = civil_from_days(days);
        (y, m as i64, d as i64, t)
    };
    let (y1, m1, d1, t1) = split(a);
    let (y2, m2, d2, t2) = split(b);
    let (mut years, mut months, mut days, mut time) = (y1 - y2, m1 - m2, d1 - d2, t1 - t2);
    if time < 0 {
        time += DAY;
        days -= 1;
    }
    if days < 0 {
        days += days_in_month(y2, m2 as u32) as i64;
        months -= 1;
    }
    if months < 0 {
        months += 12;
        years -= 1;
    }
    Interval { months: (years * 12 + months) as i32, days: days as i32, micros: time }
}

/// `i * factor`, or `i / factor`: Postgres's `interval_mul` and `interval_div`. Whole months and
/// days come from the products, and their fractions cascade down (a fraction of a month into
/// days, of a day into microseconds), never up.
pub fn scale(i: Interval, factor: f64, divide: bool) -> Result<Interval, &'static str> {
    const OUT: &str = "interval out of range";
    if divide && factor == 0.0 {
        return Err("division by zero");
    }
    let by = |x: f64| if divide { x / factor } else { x * factor };
    // to the nearest microsecond, as Postgres's TSROUND
    let round = |x: f64| (x * 1e6).round_ties_even() / 1e6;
    let whole = |x: f64| if x.is_nan() || x > i32::MAX as f64 || x < i32::MIN as f64 { Err(OUT) } else { Ok(x as i32) };
    let (months, days) = (whole(by(i.months as f64))?, whole(by(i.days as f64))?);
    let month_days = round((by(i.months as f64) - months as f64) * 30.0);
    let mut secs = round((by(i.days as f64) - days as f64 + month_days - (month_days as i32) as f64) * 86_400.0);
    let mut days = days;
    if secs.abs() >= 86_400.0 {
        days = days.checked_add((secs / 86_400.0) as i32).ok_or(OUT)?;
        secs -= (secs / 86_400.0) as i32 as f64 * 86_400.0;
    }
    let days = days.checked_add(month_days as i32).ok_or(OUT)?;
    let micros = (by(i.micros as f64) + secs * 1e6).round_ties_even();
    if !(-(2f64.powi(63))..2f64.powi(63)).contains(&micros) {
        return Err(OUT);
    }
    Ok(Interval { months, days, micros: micros as i64 })
}

/// `justify_hours`: whole days out of the time (24 hours become a day), the signs made to agree.
pub fn justify_hours(i: Interval) -> Result<Interval, &'static str> {
    let (mut days, mut micros) =
        (i.days.checked_add((i.micros / DAY) as i32).ok_or("interval out of range")?, i.micros % DAY);
    if days > 0 && micros < 0 {
        micros += DAY;
        days -= 1;
    } else if days < 0 && micros > 0 {
        micros -= DAY;
        days += 1;
    }
    Ok(Interval { months: i.months, days, micros })
}

/// `justify_days`: whole months out of the days (30 days become a month), the signs made to agree.
pub fn justify_days(i: Interval) -> Result<Interval, &'static str> {
    let (mut months, mut days) = (i.months.checked_add(i.days / 30).ok_or("interval out of range")?, i.days % 30);
    if months > 0 && days < 0 {
        days += 30;
        months -= 1;
    } else if months < 0 && days > 0 {
        days -= 30;
        months += 1;
    }
    Ok(Interval { months, days, micros: i.micros })
}

/// `justify_interval`: both, and the signs of months, days and time made to agree.
pub fn justify_interval(i: Interval) -> Result<Interval, &'static str> {
    const OUT: &str = "interval out of range";
    let (mut months, mut days, mut micros) = (i.months, i.days, i.micros);
    // days first when time would only push them further from zero (avoids overflow)
    if (days > 0 && micros > 0) || (days < 0 && micros < 0) {
        months = months.checked_add(days / 30).ok_or(OUT)?;
        days %= 30;
    }
    days = days.checked_add((micros / DAY) as i32).ok_or(OUT)?;
    micros %= DAY;
    months = months.checked_add(days / 30).ok_or(OUT)?;
    days %= 30;
    if months > 0 && (days < 0 || (days == 0 && micros < 0)) {
        days += 30;
        months -= 1;
    } else if months < 0 && (days > 0 || (days == 0 && micros > 0)) {
        days -= 30;
        months += 1;
    }
    if days > 0 && micros < 0 {
        micros += DAY;
        days -= 1;
    } else if days < 0 && micros > 0 {
        micros -= DAY;
        days += 1;
    }
    Ok(Interval { months, days, micros })
}

/// A date from the year, month and day the `make_*` functions are given: a negative year is BC (-44
/// is 44 BC), and there is no year 0. None if it isn't a date.
pub fn make_date(y: i64, m: i64, d: i64) -> Option<i32> {
    if y == 0 || !(1..=12).contains(&m) || d < 1 {
        return None;
    }
    let y = if y < 0 { y + 1 } else { y };
    if y.abs() > MAX_YEAR || d > days_in_month(y, m as u32) as i64 {
        return None;
    }
    let days = days_from_civil(y, m as u32, d as u32);
    // Postgres's dates: 4714-11-24 BC to 5874897-12-31
    (-2_440_588..=2_145_042_905).contains(&days).then_some(days as i32)
}

/// A time of day from hours, minutes and seconds (with fractions): 24:00:00 is the last moment of
/// the day, and seconds may reach 60, as Postgres allows. None if it isn't one.
pub fn make_time(h: i64, m: i64, sec: f64) -> Option<i64> {
    if h < 0 || !(0..=59).contains(&m) || !(0.0..=60.0).contains(&sec) || h > 24 || (h == 24 && (m > 0 || sec > 0.0)) {
        return None;
    }
    Some((h * 60 + m) * 60 * SECOND + (sec * SECOND as f64).round_ties_even() as i64)
}

/// An interval written with fields, `INTERVAL '1-2' YEAR TO MONTH`, `INTERVAL '1 2:03' DAY TO SECOND(2)`: `hi` and
/// `lo` are 0 year, 1 month, 2 day, 3 hour, 4 minute, 5 second. The text is read as interval_in reads it with that
/// typmod: a number with no unit is of the last field (`'1 2' DAY TO HOUR` is a day and 2 hours), and the fields after
/// `lo` are dropped and the seconds rounded to `precision` digits.
pub fn interval_by_fields(text: &str, hi: u8, lo: u8, precision: Option<u32>) -> Result<Interval, Bad> {
    parse_interval_fields(text, interval_range(hi, lo), precision)
}

/// The typmod's range of fields (the `IM_` masks) from `hi` to `lo` (0 year ... 5 second).
pub fn interval_range(hi: u8, lo: u8) -> u32 {
    const MASKS: [u32; 6] = [IM_YEAR, IM_MONTH, IM_DAY, IM_HOUR, IM_MINUTE, IM_SECOND];
    (hi..=lo.max(hi)).fold(0, |m, f| m | MASKS[usize::from(f.min(5))])
}

/// An interval cast to a typmod (`::interval day to minute`): the fields after the range's last dropped, and the
/// seconds rounded to `precision` digits (AdjustIntervalForTypmod).
pub fn fit_interval(mut i: Interval, range: u32, precision: Option<u32>) -> Interval {
    decode::adjust_for_typmod(&mut i, range, precision);
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        for d in [-800_000, -1, 0, 59, 11_016, 19_000, 2_932_896] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(parse_date("2024-02-29").map(format_date).ok().as_deref(), Some("2024-02-29"));
        assert_eq!(parse_date("2023-02-29"), Err(Bad::Field));
        assert_eq!(parse_date("2024-13-01"), Err(Bad::MdField)); // (with the datestyle hint, as Postgres)
        assert_eq!(parse_date("2024-xx-01"), Err(Bad::Syntax));
        assert_eq!(parse_date("5874898-01-01"), Err(Bad::Range));
        assert_eq!(parse_timestamp("300000-01-01", false), Err(Bad::Range));
    }

    #[test]
    fn timestamps() {
        let t = parse_timestamp("2024-03-10 14:05:09.25", false).unwrap();
        assert_eq!(format_timestamp(t, false, ' '), "2024-03-10 14:05:09.25");
        assert_eq!(parse_timestamp("2024-03-10T14:05:09.25Z", true), Ok(t));
        assert_eq!(
            format_timestamp(parse_timestamp("2024-03-10 16:05:09.25+02", true).unwrap(), true, ' '),
            "2024-03-10 14:05:09.25+00"
        );
        assert_eq!(
            parse_timestamp("2024-03-10 16:05:09.25+02:00", false),
            parse_timestamp("2024-03-10 16:05:09.25", false)
        );
        assert_eq!(
            format_timestamp(parse_timestamp("1969-12-31 23:59:59", false).unwrap(), false, ' '),
            "1969-12-31 23:59:59"
        );
        assert_eq!(parse_timestamp("2024-03-10", false), Ok(parse_date("2024-03-10").unwrap() as i64 * DAY));
        assert_eq!(parse_timestamp("2024-03-10 25:00", false), Err(Bad::Field));
    }

    #[test]
    fn intervals() {
        let i = parse_interval("1 year 2 mons 3 days 04:05:06").unwrap();
        assert_eq!((i.months, i.days, i.micros), (14, 3, 14_706_000_000));
        assert_eq!(format_interval(i), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(format_interval(parse_interval("90 minutes").unwrap()), "01:30:00");
        assert_eq!(format_interval(parse_interval("-1 day").unwrap()), "-1 days");
        assert_eq!(format_interval(parse_interval("1 week 2h").unwrap()), "7 days 02:00:00");
        assert_eq!(format_interval(Interval::default()), "00:00:00");
        assert_eq!(parse_interval("1.5 months"), Some(Interval { months: 1, days: 15, micros: 0 }));
        assert_eq!(parse_interval("3 fortnights"), None);
        // calendar months: Jan 31 + 1 month is the last day of February
        let jan31 = parse_timestamp("2024-01-31 10:00", false).unwrap();
        let feb = add_interval(jan31, parse_interval("1 month").unwrap()).unwrap();
        assert_eq!(add_interval(i64::MAX - DAY, parse_interval("2 days").unwrap()), None);
        assert_eq!(format_timestamp(feb, false, ' '), "2024-02-29 10:00:00");
        assert_eq!(format_interval(between(feb, jan31)), "29 days");
    }

    #[test]
    fn trunc_and_extract() {
        let t = parse_timestamp("2024-05-15 13:45:30.5", false).unwrap(); // a Wednesday
        let f = |u: &str| format_timestamp(trunc(u, t).unwrap(), false, ' ');
        assert_eq!(f("hour"), "2024-05-15 13:00:00");
        assert_eq!(f("day"), "2024-05-15 00:00:00");
        assert_eq!(f("week"), "2024-05-13 00:00:00");
        assert_eq!(f("month"), "2024-05-01 00:00:00");
        assert_eq!(f("quarter"), "2024-04-01 00:00:00");
        assert_eq!(f("year"), "2024-01-01 00:00:00");
        // (fields are numerics, at the scale Postgres writes each with)
        let x = |field: &str, us: i64| match extract_numeric(field, Source::Timestamp, us, us, 0) {
            Field::Value(d) => Some(d.to_string()),
            _ => None,
        };
        assert_eq!(x("dow", t).as_deref(), Some("3"));
        assert_eq!(x("second", t).as_deref(), Some("30.500000"));
        assert_eq!(x("doy", t).as_deref(), Some("136"));
        assert_eq!(x("epoch", parse_timestamp("1970-01-02", false).unwrap()).as_deref(), Some("86400.000000"));
    }
}
