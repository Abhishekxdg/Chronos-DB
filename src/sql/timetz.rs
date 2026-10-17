//! time with time zone: a time of day and the offset it is written in, `15:36:39-05`, kept as that text.

use super::funcs::Scalar;
use super::time::{self, Bad};
use super::{SqlError, Value, err};

type Result<T> = std::result::Result<T, SqlError>;

pub(super) fn bad(s: &str, b: Bad) -> SqlError {
    match b {
        Bad::Syntax => SqlError {
            code: "22007",
            message: format!("invalid input syntax for type time with time zone: \"{s}\""),
            info: None,
        },
        Bad::Zone => {
            SqlError { code: "22009", message: format!("time zone displacement out of range: \"{s}\""), info: None }
        }
        Bad::NoZone(z) => SqlError { code: "22023", message: format!("time zone \"{z}\" not recognized"), info: None },
        _ => SqlError { code: "22008", message: format!("date/time field value out of range: \"{s}\""), info: None },
    }
}

/// The time of day (µs from midnight) and the offset (seconds east of UTC) of `15:36:39 PDT`, `13:30-04`,
/// `T040506.789+08`, `2003-03-07 15:36:39 America/New_York` (a named zone only with a date, which says which of
/// its offsets), as timetz_in reads them.
pub(super) fn parse(s: &str) -> std::result::Result<(i64, i32), Bad> {
    match time::parse_timetz(s) {
        Err(Bad::Syntax) => parse_loose(s).map_err(|_| Bad::Syntax),
        r => r,
    }
}

fn parse_loose(s: &str) -> std::result::Result<(i64, i32), Bad> {
    let words: Vec<&str> = s.split_whitespace().collect();
    let at =
        words.iter().position(|w| w.starts_with(|c: char| c.is_ascii_digit()) && w.contains(':')).ok_or(Bad::Syntax)?;
    let date = if at > 0 { time::parse_date(&words[..at].join(" ")).ok() } else { None };
    let mut tword = words[at].to_string();
    let mut zone: Option<String> = None;
    // 13:30:25.575401-04:30: the offset stuck on the time
    if let Some(i) = tword.find(['+', '-']) {
        zone = Some(tword[i..].to_string());
        tword.truncate(i);
    }
    let mut rest: Vec<&str> = words[at + 1..].to_vec();
    let mut ampm = None;
    if let Some(w) = rest.first().copied()
        && (w.eq_ignore_ascii_case("am") || w.eq_ignore_ascii_case("pm"))
    {
        ampm = Some(w.to_ascii_uppercase());
        rest.remove(0);
    }
    if tword.len() >= 2 && (tword.to_ascii_lowercase().ends_with("am") || tword.to_ascii_lowercase().ends_with("pm")) {
        tword.truncate(tword.len() - 2);
    }
    let ztext = match (&zone, rest.as_slice()) {
        (Some(z), []) => Some(z.clone()),
        (None, []) => None,
        (None, [z]) => Some((*z).to_string()),
        // (more after the zone, `15:36:39 MSK m2`, is not a time with time zone)
        (None, _) | (Some(_), _) => return Err(Bad::Syntax),
    };
    let timestr = match ampm {
        Some(m) => format!("{tword} {m}"),
        None => tword,
    };
    let us = time::parse_time_checked(&timestr)?;
    let off = match ztext {
        None => (time::offset() / time::SECOND) as i32,
        Some(z) => zone_offset(&z, date, us)?,
    };
    Ok((us, off))
}

fn zone_offset(z: &str, date: Option<i32>, us: i64) -> std::result::Result<i32, Bad> {
    if z.eq_ignore_ascii_case("z") || z.eq_ignore_ascii_case("utc") {
        return Ok(0);
    }
    if let Some(sign) = z.strip_prefix(['+', '-']) {
        let neg = z.starts_with('-');
        let parts: Vec<&str> = sign.split(':').collect();
        let num = |p: &str| p.parse::<i32>().map_err(|_| Bad::Syntax);
        let (h, m, sec) = match parts.as_slice() {
            [h] if h.len() == 4 => (num(&h[..2])?, num(&h[2..])?, 0),
            [h] => (num(h)?, 0, 0),
            [h, m] => (num(h)?, num(m)?, 0),
            [h, m, s] => (num(h)?, num(m)?, num(s)?),
            _ => return Err(Bad::Syntax),
        };
        if h > 15 || m > 59 || sec > 59 {
            return Err(Bad::Zone);
        }
        let t = h * 3600 + m * 60 + sec;
        return Ok(if neg { -t } else { t });
    }
    if let Some((secs, _)) = super::tz::abbrev(z) {
        return Ok(secs);
    }
    if let (Some(d), Some(rules)) = (date, super::tz::find(z)) {
        let wall = i64::from(d) * time::DAY + us;
        let utc = rules.utc(wall.div_euclid(time::SECOND)).saturating_mul(time::SECOND) + wall.rem_euclid(time::SECOND);
        return Ok(((wall - utc) / time::SECOND) as i32);
    }
    Err(Bad::Syntax)
}

pub(super) fn format(us: i64, off: i32) -> String {
    format!("{}{}", time::format_time(us), time::format_offset(off))
}

/// Text as (µs, offset east in seconds).
pub(super) fn read(s: &str) -> Result<(i64, i32)> {
    let (t, z) = s.split_at(s.rfind(['+', '-']).unwrap_or(s.len()));
    let us = time::parse_time(t).ok_or_else(|| bad(s, Bad::Syntax))?;
    let off = zone_offset(z, None, us).map_err(|b| bad(s, b))?;
    Ok((us, off))
}

/// UTC micros of the time, then the zone (west positive, as Postgres compares), for ordering and equality.
fn key(us: i64, off: i32) -> (i64, i32) {
    (us - i64::from(off) * time::SECOND, -off)
}

pub(super) fn sort_key(s: &str) -> Result<i64> {
    let (us, off) = read(s)?;
    let (u, z) = key(us, off);
    Ok(u * 100_000 + i64::from(z / 900) + 50_000)
}

pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    use Value::{Bool, Null, Text};
    if a.contains(&Null) {
        return Ok(Null);
    }
    let Scalar::TimeTzFn(k) = f else { return err("XX000", "not a timetz function") };
    Ok(match k {
        // 0..=5: = <> < > <= >=
        0..=5 => {
            let (x, y) = (read(&a[0].to_text())?, read(&a[1].to_text())?);
            let o = key(x.0, x.1).cmp(&key(y.0, y.1));
            Bool(match k {
                0 => o.is_eq(),
                1 => o.is_ne(),
                2 => o.is_lt(),
                3 => o.is_gt(),
                4 => o.is_le(),
                _ => o.is_ge(),
            })
        }
        // timetz + interval, timetz - interval
        6 | 7 => {
            let (us, off) = read(&a[0].to_text())?;
            let Value::Interval(i) = &a[1] else { return err("42883", "timetz arithmetic takes an interval") };
            let d = if k == 6 { i.micros } else { -i.micros };
            Text(format((us + d).rem_euclid(time::DAY), off))
        }
        // timetz as a time
        8 => Text(time::format_time(read(&a[0].to_text())?.0)),
        // a time, or a timestamptz, as a timetz in the session's zone
        9 => match &a[0] {
            Value::TimestampTz(t) => {
                let off = time::offset_at(*t);
                Text(format((*t + off).rem_euclid(time::DAY), (off / time::SECOND) as i32))
            }
            v => {
                let us = match v {
                    Value::Time(t) => *t,
                    v => time::parse_time(&v.to_text()).ok_or_else(|| bad(&v.to_text(), Bad::Syntax))?,
                };
                Text(format(us, (time::offset() / time::SECOND) as i32))
            }
        },
        // AT TIME ZONE: the zone as an offset east, or an interval
        10 => {
            let (us, off) = read(&a[0].to_text())?;
            let target = match &a[1] {
                // (an interval is the offset east, `INTERVAL '-10:00'` ten hours west, as timetz_izone takes it)
                Value::Interval(i) if i.months != 0 || i.days != 0 => {
                    return err(
                        "22023",
                        format!("interval time zone \"{}\" must not include months or days", time::format_interval(*i)),
                    );
                }
                Value::Interval(i) => (i.micros / time::SECOND) as i32,
                v => {
                    let name = v.to_text();
                    // POSIX style: UTC+10 is ten hours west
                    if let Some(h) = name
                        .strip_prefix("UTC")
                        .or_else(|| name.strip_prefix("GMT"))
                        .filter(|r| r.starts_with(['+', '-']))
                    {
                        let (sign, d) = h.split_at(1);
                        let n: i32 = d.parse().unwrap_or(0);
                        if sign == "+" { -n * 3600 } else { n * 3600 }
                    } else {
                        zone_offset(&name, None, us).map_err(|_| SqlError {
                            code: "22023",
                            message: format!("time zone \"{name}\" not recognized"),
                            info: None,
                        })?
                    }
                }
            };
            let utc = us - i64::from(off) * time::SECOND;
            let local = (utc + i64::from(target) * time::SECOND).rem_euclid(time::DAY);
            Text(format(local, target))
        }
        // date + timetz: the moment that time of day, in its offset, is on that date
        12 => {
            let d = match &a[0] {
                Value::Date(d) => *d,
                v => time::parse_date(&v.to_text()).map_err(|b| bad(&v.to_text(), b))?,
            };
            if time::infinite_date(d) {
                return err("22008", "timestamp out of range");
            }
            let (us, off) = read(&a[1].to_text())?;
            let t = i64::from(d)
                .checked_mul(time::DAY)
                .and_then(|x| x.checked_add(us))
                .and_then(|x| x.checked_sub(i64::from(off) * time::SECOND));
            match t {
                Some(t) => Value::TimestampTz(t),
                None => return err("22008", "timestamp out of range"),
            }
        }
        // extract(field FROM timetz)
        _ => {
            let (us, off) = read(&a[0].to_text())?;
            let field = a[1].to_text().to_lowercase();
            let (h, m, sec_us) = (us / time::HOUR, us % time::HOUR / time::MINUTE, us % time::MINUTE);
            let secs = |x: i64| {
                Value::Numeric(
                    super::decimal::Dec::parse(&format!("{}.{:06}", x / 1_000_000, x % 1_000_000))
                        .unwrap_or(super::decimal::Dec::int(0)),
                )
            };
            match field.as_str() {
                "microsecond" | "microseconds" => Value::Numeric(super::decimal::Dec::int(sec_us)),
                "millisecond" | "milliseconds" => Value::Numeric(
                    super::decimal::Dec::parse(&format!("{}.{:03}", sec_us / 1000, sec_us % 1000))
                        .unwrap_or(super::decimal::Dec::int(0)),
                ),
                "second" | "seconds" => secs(sec_us),
                "minute" | "minutes" => Value::Numeric(super::decimal::Dec::int(m)),
                "hour" | "hours" => Value::Numeric(super::decimal::Dec::int(h)),
                "timezone" => Value::Numeric(super::decimal::Dec::int(i64::from(off))),
                "timezone_hour" => Value::Numeric(super::decimal::Dec::int(i64::from(off / 3600))),
                "timezone_minute" => Value::Numeric(super::decimal::Dec::int(i64::from(off % 3600 / 60))),
                "epoch" => secs((us - i64::from(off) * time::SECOND).rem_euclid(time::DAY * 2)),
                "day" | "month" | "year" | "decade" | "century" | "millennium" | "dow" | "doy" | "isodow"
                | "isoyear" | "quarter" | "week" | "julian" => {
                    return err("0A000", format!("unit \"{field}\" not supported for type time with time zone"));
                }
                _ => return err("22023", format!("unit \"{field}\" not recognized for type time with time zone")),
            }
        }
    })
}
