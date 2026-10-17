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
