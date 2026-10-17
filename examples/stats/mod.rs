//! Measurements shared by the worlds, simulate and mcts examples: memory, CPU, disk, percentiles.
#![allow(dead_code)]

use std::time::{Duration, Instant};

/// This process's memory, in MB: the physical footprint on macOS (what `top` shows as MEM,
/// including compressed and swapped pages), resident memory elsewhere.
pub fn mem_mb() -> f64 {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut ri: libc::rusage_info_v2 = std::mem::zeroed();
        if libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, &mut ri as *mut _ as *mut _) == 0 {
            return ri.ri_phys_footprint as f64 / 1e6;
        }
    }
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output();
    out.ok().and_then(|o| String::from_utf8(o.stdout).ok()).and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.0)
        / 1024.0
}

/// CPU seconds this process has used so far, user + system.
pub fn cpu_s() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    s(u.ru_utime) + s(u.ru_stime)
}

/// The 1-minute load average, as `uptime` prints it.
pub fn load() -> f64 {
    let mut l = [0f64; 3];
    unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    l[0]
}

pub fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// The p-th percentile (0..=100) of sorted durations, in microseconds.
pub fn pct(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p / 100.0).round() as usize;
    sorted[i].as_secs_f64() * 1e6
}

pub fn dir_size(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => dir_size(&e.path()),
                    Ok(m) => m.len(),
                    _ => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

/// A small deterministic generator (splitmix64): the same seed gives the same numbers anywhere.
#[derive(Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    /// uniform in [0, 1)
    pub fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}
