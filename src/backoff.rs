//! Failed logins, per client address, for both servers (the Postgres port and the HTTP API): after
//! `FREE` wrong passwords or tokens (CHRONOS_LOGIN_FREE_TRIES changes how many), an address waits before its next try, a wait that doubles with
//! each further failure up to `CAP`, so guessing a token is slow however many connections it
//! takes. An address's failures are forgotten `FORGET` after its last one (not when one of its
//! logins succeeds: a client holding one good credential could otherwise go on guessing others at
//! full speed). In memory only: a restart forgets them.
//!
//!   try ─> `wait(ip)`: Some(left) ─> refused at once (Postgres 28P01, HTTP 429), saying how long
//!          None ─> the login's own check ─> wrong: `failed(ip)`

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Wrong tries an address gets before it must wait: CHRONOS_LOGIN_FREE_TRIES, else `FREE`.
static FREE_TRIES: LazyLock<u32> = LazyLock::new(|| crate::http::env("CHRONOS_LOGIN_FREE_TRIES", FREE));
const FREE: u32 = 10;
/// The first wait, which doubles with each failure after it.
const FIRST: Duration = Duration::from_secs(1);
/// The longest wait.
const CAP: Duration = Duration::from_secs(60);
/// An address's failures are forgotten this long after its last one.
const FORGET: Duration = Duration::from_secs(15 * 60);
/// The addresses kept at most: past it, those quiet longest go first.
const MOST: usize = 65_536;

/// Per address: the failures counted, and when the last one was.
static FAILS: LazyLock<Mutex<HashMap<IpAddr, (u32, Instant)>>> = LazyLock::new(Default::default);

/// How long an address waits after `n` failures.
fn delay(n: u32) -> Duration {
    after(n, *FREE_TRIES)
}

/// How long an address waits after `n` failures when `free` are free.
fn after(n: u32, free: u32) -> Duration {
    match n.saturating_sub(free) {
        0 => Duration::ZERO,
        k => FIRST.saturating_mul(1u32 << (k - 1).min(16)).min(CAP),
    }
}

/// The wait left before `ip` may try to log in again (None: it may now).
pub fn wait(ip: Option<IpAddr>) -> Option<Duration> {
    let ip = ip?;
    let m = FAILS.lock().unwrap_or_else(|p| p.into_inner());
    let (n, last) = *m.get(&ip)?;
    let left = (last + delay(n)).saturating_duration_since(Instant::now());
    (!left.is_zero()).then_some(left)
}

/// A wrong password or token from `ip`.
pub fn failed(ip: Option<IpAddr>) {
    let Some(ip) = ip else { return };
    let mut m = FAILS.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    if m.len() >= MOST && !m.contains_key(&ip) {
        m.retain(|_, (_, last)| now.duration_since(*last) < FORGET);
        if m.len() >= MOST {
            // (still full: the quieter half goes)
            let mut ages: Vec<Instant> = m.values().map(|(_, l)| *l).collect();
            ages.sort();
            let cut = ages[ages.len() / 2];
            m.retain(|_, (_, last)| *last > cut);
        }
    }
    let e = m.entry(ip).or_insert((0, now));
    let n = if now.duration_since(e.1) >= FORGET { 0 } else { e.0 };
    *e = (n.saturating_add(1), now);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_double_after_the_free_tries_up_to_the_cap() {
        let delay = |n| after(n, FREE);
        assert_eq!((0..=FREE).map(delay).max(), Some(Duration::ZERO));
        assert_eq!(delay(FREE + 1), FIRST);
        assert_eq!(delay(FREE + 2), FIRST * 2);
        assert_eq!(delay(FREE + 3), FIRST * 4);
        assert_eq!(delay(FREE + 40), CAP);
        assert_eq!(delay(u32::MAX), CAP);
        if *FREE_TRIES != FREE {
            return; // (CHRONOS_LOGIN_FREE_TRIES is set)
        }
        let ip: Option<IpAddr> = Some("192.0.2.7".parse().unwrap());
        for _ in 0..FREE {
            assert_eq!(wait(ip), None);
            failed(ip);
        }
        assert_eq!(wait(ip), None, "the free tries");
        failed(ip);
        assert!(wait(ip).is_some_and(|w| w <= FIRST && !w.is_zero()));
        assert_eq!(wait(Some("192.0.2.8".parse().unwrap())), None, "per address");
        assert_eq!(wait(None), None);
    }
}
