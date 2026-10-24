//! The process's limits on what a server holds open: how many client connections its descriptors
//! allow (`max_connections`, as `SHOW max_connections` and the listeners read it).

use std::sync::LazyLock;

/// Descriptors a connection may hold at once (its socket's handles, a pipe, a login deadline's,
/// TLS's), and the ones the process keeps for itself (log, pages, listeners, backups, spills).
const FDS_PER_CONN: u64 = 8;
const FDS_RESERVED: u64 = 256;

/// A number from the environment variable `name`, or `default`.
fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The process's limit on open descriptors (`ulimit -n`). Its soft limit starts at 1024 on most
/// systems, which ~140 connections use up; it is moved up to the hard limit (at most 1 Mi, the
/// kernel's usual ceiling; macOS refuses more than 10240), and what it came to is returned: the
/// soft limit as it was, if the system refused the raise.
fn nofile() -> u64 {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: fills in a struct we own
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return 1024;
    }
    #[allow(clippy::unnecessary_cast)] // (u32 on some targets)
    let (cur, max) = (lim.rlim_cur as u64, lim.rlim_max as u64);
    let want = if cfg!(target_os = "macos") { max.min(10240) } else { max.min(1 << 20) };
    if want <= cur {
        return cur;
    }
    let new = libc::rlimit { rlim_cur: want as libc::rlim_t, rlim_max: lim.rlim_max };
    // SAFETY: a struct we own, read
    match unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) } {
        0 => want,
        _ => cur,
    }
}

/// The descriptor limit in effect for serving: raised once (see `nofile`), and what that came to,
/// so a raise the system refused leaves the cap at the limit really there.
static FDS: LazyLock<u64> = LazyLock::new(nofile);

/// Connections served at once, across the listeners (HTTP and Postgres together); more are
/// refused (`53300` / `503`). CHRONOS_MAX_CONNECTIONS (1000), but no more than the process's
/// descriptors hold (`ulimit -n`, once raised): a limit that can't be reached is not one.
/// `SHOW max_connections` says this.
pub fn max_connections() -> usize {
    static N: LazyLock<usize> = LazyLock::new(|| {
        let asked: usize = env("CHRONOS_MAX_CONNECTIONS", 1000);
        let fits = (FDS.saturating_sub(FDS_RESERVED) / FDS_PER_CONN).max(1);
        asked.min(usize::try_from(fits).unwrap_or(usize::MAX))
    });
    *N
}

/// Raises the descriptor limit to what serving connections needs, once, and says so if the limit
/// still cuts `max_connections` below what was asked for.
pub fn prepare_descriptors() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let soft = *FDS;
        let asked: usize = env("CHRONOS_MAX_CONNECTIONS", 1000);
        let limit = max_connections();
        if limit < asked {
            eprintln!(
                "chronos: max_connections is {limit}, not {asked}: the process may hold {soft} open files \
                 (raise `ulimit -n`)"
            );
        }
    });
}

/// Ceilings the operator puts on a database's own settings (CHRONOS_LIMITS): what its admin
/// can't raise with `ALTER SYSTEM`, for a host that sells the database by plan. 0: none. With a
/// ceiling, a setting left at 0 (no limit) is the ceiling, and one above it can't be set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ceilings {
    pub max_worlds: u64,
    pub history_retention_ms: u64,
    pub world_idle_ttl_ms: u64,
    pub statement_timeout_ms: u64,
}

impl Ceilings {
    /// The file CHRONOS_LIMITS names, if it's set: lines of `key = integer` (`#` comments,
    /// `_` between digits, as TOML writes them). A key it doesn't know, or a value that isn't a
    /// whole number, is an error: a ceiling misspelled would be no ceiling at all.
    pub fn from_env() -> std::io::Result<Ceilings> {
        match std::env::var_os("CHRONOS_LIMITS") {
            None => Ok(Ceilings::default()),
            Some(p) => {
                let text = std::fs::read_to_string(&p).map_err(|e| {
                    std::io::Error::new(e.kind(), format!("CHRONOS_LIMITS={}: {e}", p.to_string_lossy()))
                })?;
                Ceilings::parse(&text)
                    .map_err(|e| std::io::Error::other(format!("CHRONOS_LIMITS={}: {e}", p.to_string_lossy())))
            }
        }
    }

    pub fn parse(text: &str) -> Result<Ceilings, String> {
        let mut c = Ceilings::default();
        for (i, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            let at = || format!("line {}", i + 1);
            let (k, v) = line.split_once('=').ok_or_else(|| format!("{}: expected key = integer", at()))?;
            let v = v.trim();
            let n: u64 = match v.starts_with(|c: char| c.is_ascii_digit()) && !v.ends_with('_') && !v.contains("__") {
                true => v.replace('_', "").parse().ok(),
                false => None,
            }
            .ok_or_else(|| format!("{}: {} must be a whole number, not {v}", at(), k.trim()))?;
            let slot = match k.trim().trim_matches('"') {
                "max_worlds" => &mut c.max_worlds,
                "history_retention_ms" => &mut c.history_retention_ms,
                "world_idle_ttl_ms" => &mut c.world_idle_ttl_ms,
                "statement_timeout_ms" => &mut c.statement_timeout_ms,
                k => {
                    return Err(format!(
                        "{}: no limit named {k} (max_worlds, history_retention_ms, world_idle_ttl_ms, \
                         statement_timeout_ms)",
                        at()
                    ));
                }
            };
            *slot = n;
        }
        Ok(c)
    }
}

/// `v` under ceiling `most` (0: none): with a ceiling, 0 (no limit) and anything above it are the
/// ceiling.
pub fn ceil(v: u64, most: u64) -> u64 {
    match most {
        0 => v,
        m if v == 0 || v > m => m,
        _ => v,
    }
}

/// CHRONOS_MAX_DISK_BYTES: the most the database folder may hold, in bytes (0 or unset: no cap).
pub fn max_disk_bytes_from_env() -> std::io::Result<u64> {
    match std::env::var("CHRONOS_MAX_DISK_BYTES") {
        Err(_) => Ok(0),
        Ok(v) => v
            .trim()
            .parse()
            .map_err(|_| std::io::Error::other(format!("CHRONOS_MAX_DISK_BYTES must be a number of bytes, not {v:?}"))),
    }
}

/// CHRONOS_MAX_MEMORY_BYTES: the most resident memory the process may hold before it refuses new
/// statements (0 or unset: no cap). A soft, process-wide cap — a statement already running is not
/// interrupted; the next one is refused while the process is over (see `Core::guard_memory`).
pub fn max_memory_bytes_from_env() -> std::io::Result<u64> {
    match std::env::var("CHRONOS_MAX_MEMORY_BYTES") {
        Err(_) => Ok(0),
        Ok(v) => v.trim().parse().map_err(|_| {
            std::io::Error::other(format!("CHRONOS_MAX_MEMORY_BYTES must be a number of bytes, not {v:?}"))
        }),
    }
}

/// The process's current resident set size in bytes, or None if it can't be read. Linux reads
/// `/proc/self/statm` (its second field is resident pages); macOS asks the Mach kernel for the
/// task's `resident_size`. Cheap enough to call once per statement.
#[cfg(target_os = "linux")]
pub fn process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf is a pure lookup of a system constant.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page > 0).then(|| pages * page as u64)
}

/// See the Linux variant. macOS: `task_info(MACH_TASK_BASIC_INFO)`. (`libc`'s mach bindings are
/// deprecated in favour of the `mach2` crate; a single `task_info` call doesn't warrant the extra
/// dependency, so the deprecation is allowed here.)
#[cfg(target_os = "macos")]
#[allow(deprecated)]
pub fn process_rss_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::zeroed();
    let mut count = (std::mem::size_of::<libc::mach_task_basic_info>() / std::mem::size_of::<libc::integer_t>())
        as libc::mach_msg_type_number_t;
    // SAFETY: mach_task_self() is the current task; task_info fills `info`, writing at most
    // `count` integer_t words into the struct we own, and reports how many it wrote.
    let rc = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO as libc::task_flavor_t,
            info.as_mut_ptr() as libc::task_info_t,
            &mut count,
        )
    };
    if rc != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: task_info returned KERN_SUCCESS, so `info` is initialised.
    Some(unsafe { info.assume_init() }.resident_size)
}

/// Bytes as people read them: `512 MB`, `1.5 GB`, `64 kB`.
pub fn show_bytes(n: u64) -> String {
    let (unit, name) = match n {
        n if n >= 1 << 30 => (1u64 << 30, "GB"),
        n if n >= 1 << 20 => (1 << 20, "MB"),
        n if n >= 1 << 10 => (1 << 10, "kB"),
        _ => return format!("{n} bytes"),
    };
    let x = format!("{:.1}", n as f64 / unit as f64);
    format!("{} {name}", x.strip_suffix(".0").unwrap_or(&x))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceilings_parse() {
        let c = Ceilings::parse("# free plan\nmax_worlds = 10\nhistory_retention_ms = 86_400_000 # a day\n\n").unwrap();
        assert_eq!(c, Ceilings { max_worlds: 10, history_retention_ms: 86_400_000, ..Default::default() });
        assert!(Ceilings::parse("max_world = 1").unwrap_err().contains("no limit named max_world"));
        assert!(Ceilings::parse("max_worlds = -1").is_err());
        assert!(Ceilings::parse("max_worlds = '10'").is_err());
        assert!(Ceilings::parse("max_worlds").is_err());
        assert_eq!((ceil(0, 10), ceil(20, 10), ceil(5, 10), ceil(7, 0)), (10, 10, 5, 7));
        assert_eq!(
            (show_bytes(512 << 20), show_bytes(3 << 29), show_bytes(2_000)),
            ("512 MB".into(), "1.5 GB".into(), "2 kB".into())
        );
    }
}
