//! What the database has done since it was opened: how many of each operation, how long they took
//! (p50, p95, p99, max) and a few counters. Read with `Core::metrics`, `SHOW METRICS` or, from
//! `chronos serve`, `GET /metrics` in Prometheus' text format.
//!
//! Each operation keeps a histogram of atomic counters, so recording is a few relaxed adds and
//! never takes a lock. Buckets are an eighth of a power of two wide: a quantile is within 6% of
//! the true value.

use std::sync::atomic::{AtomicU64, Ordering}; // not `crate::sync`: counters are no part of what the simulator checks
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Select,
    Insert,
    Update,
    Delete,
    Schema, // CREATE, ALTER, DROP
    Sql,    // any other statement
    Commit, // a transaction's changes going into its branch
    Sync,   // waiting for the log to be on disk
    Fork,
    Diff,
    Merge,
    Search,
    Checkpoint,
    Gc,
    Simulate, // SIMULATE and REPLAY WORLD, whole
}

pub const OPS: [Op; 15] = [
    Op::Select,
    Op::Insert,
    Op::Update,
    Op::Delete,
    Op::Schema,
    Op::Sql,
    Op::Commit,
    Op::Sync,
    Op::Fork,
    Op::Diff,
    Op::Merge,
    Op::Search,
    Op::Checkpoint,
    Op::Gc,
    Op::Simulate,
];

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Select => "select",
            Op::Insert => "insert",
            Op::Update => "update",
            Op::Delete => "delete",
            Op::Schema => "schema",
            Op::Sql => "other_sql",
            Op::Commit => "commit",
            Op::Sync => "log_sync",
            Op::Fork => "fork",
            Op::Diff => "diff",
            Op::Merge => "merge",
            Op::Search => "search",
            Op::Checkpoint => "checkpoint",
            Op::Gc => "gc",
            Op::Simulate => "simulate",
        }
    }
}

const BUCKETS: usize = 16 + 60 * 8; // exact below 16 µs, then 8 per power of two

fn bucket(us: u64) -> usize {
    if us < 16 {
        return us as usize;
    }
    let o = 63 - us.leading_zeros() as usize; // 4..=63
    16 + (o - 4) * 8 + ((us >> (o - 3)) & 7) as usize
}

/// The middle of bucket `i`, in µs.
fn middle(i: usize) -> f64 {
    if i < 16 {
        return i as f64;
    }
    let (o, sub) = ((i - 16) / 8 + 4, (i - 16) % 8);
    let width = 1u64 << (o - 3);
    ((8 + sub as u64) * width) as f64 + (width as f64 - 1.0) / 2.0
}

struct Hist {
    counts: Vec<AtomicU64>,
    sum: AtomicU64, // µs
    max: AtomicU64, // µs
}

impl Hist {
    fn new() -> Hist {
        Hist {
            counts: (0..BUCKETS).map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicU64::new(0),
            max: AtomicU64::new(0),
        }
    }
}

pub struct Timer<'a>(&'a Metrics, Op, Instant);

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        self.0.record(self.1, self.2);
    }
}

/// One operation's numbers. Times are in milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Timing {
    pub op: &'static str,
    pub count: u64,
    pub total_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

pub struct Metrics {
    hists: Vec<Hist>,
    pub(crate) rows_written: AtomicU64,
    pub(crate) errors: AtomicU64,
    pub(crate) conflicts: AtomicU64,
    started: Instant,
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics {
            hists: OPS.iter().map(|_| Hist::new()).collect(),
            rows_written: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            conflicts: AtomicU64::new(0),
            started: Instant::now(),
        }
    }
}

impl Metrics {
    pub fn record(&self, op: Op, since: Instant) {
        let us = since.elapsed().as_micros().min(u64::MAX as u128) as u64;
        let h = &self.hists[op as usize];
        h.counts[bucket(us)].fetch_add(1, Ordering::Relaxed);
        h.sum.fetch_add(us, Ordering::Relaxed);
        h.max.fetch_max(us, Ordering::Relaxed);
    }

    /// Records how long until the returned guard is dropped.
    pub fn time(&self, op: Op) -> Timer<'_> {
        Timer(self, op, Instant::now())
    }

    /// An error returned to a client; `code` is its SQLSTATE.
    pub fn error(&self, code: &str) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        if code == "40001" {
            self.conflicts.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The operations that have run at least once.
    pub fn timings(&self) -> Vec<Timing> {
        OPS.iter()
            .zip(&self.hists)
            .filter_map(|(op, h)| {
                let counts: Vec<u64> = h.counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
                let n: u64 = counts.iter().sum();
                if n == 0 {
                    return None;
                }
                let max = h.max.load(Ordering::Relaxed) as f64;
                let at = |q: f64| {
                    let rank = ((q * n as f64).ceil() as u64).max(1);
                    let mut seen = 0;
                    for (i, c) in counts.iter().enumerate() {
                        seen += c;
                        if seen >= rank {
                            return middle(i).min(max) / 1e3;
                        }
                    }
                    max / 1e3
                };
                Some(Timing {
                    op: op.name(),
                    count: n,
                    total_ms: h.sum.load(Ordering::Relaxed) as f64 / 1e3,
                    p50_ms: at(0.5),
                    p95_ms: at(0.95),
                    p99_ms: at(0.99),
                    max_ms: max / 1e3,
                })
            })
            .collect()
    }

    /// Timings and `counters` (from `Core::counters`) in Prometheus' text format.
    pub fn prometheus(&self, counters: &[(&str, u64)]) -> String {
        let mut out = String::from("# TYPE chronos_op_seconds summary\n");
        for t in self.timings() {
            for (q, ms) in [("0.5", t.p50_ms), ("0.95", t.p95_ms), ("0.99", t.p99_ms), ("1", t.max_ms)] {
                out += &format!("chronos_op_seconds{{op=\"{}\",quantile=\"{q}\"}} {}\n", t.op, ms / 1e3);
            }
            out += &format!("chronos_op_seconds_sum{{op=\"{}\"}} {}\n", t.op, t.total_ms / 1e3);
            out += &format!("chronos_op_seconds_count{{op=\"{}\"}} {}\n", t.op, t.count);
        }
        for (name, n) in counters {
            let (kind, name) = match *name {
                "rows_written" | "errors" | "conflicts" | "cache_hits" | "cache_misses" => {
                    ("counter", format!("{name}_total"))
                }
                _ => ("gauge", name.to_string()),
            };
            out += &format!("# TYPE chronos_{name} {kind}\nchronos_{name} {n}\n");
        }
        out
    }

    pub fn uptime_secs(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_tight_and_ordered() {
        let mut last = 0;
        for us in (0..100_000u64).chain([1 << 40, u64::MAX]) {
            let b = bucket(us);
            assert!(b >= last && b < BUCKETS, "{us}");
            last = b;
            let m = middle(b);
            assert!((m - us as f64).abs() <= us as f64 / 16.0 + 0.5, "{us} in a bucket around {m}");
        }
    }

    #[test]
    fn quantiles() {
        let m = Metrics::default();
        let h = &m.hists[Op::Select as usize];
        for us in 1..=1000u64 {
            h.counts[bucket(us)].fetch_add(1, Ordering::Relaxed);
            h.max.fetch_max(us, Ordering::Relaxed);
        }
        let t = &m.timings()[0];
        assert_eq!((t.op, t.count), ("select", 1000));
        for (got, want) in [(t.p50_ms, 0.5), (t.p95_ms, 0.95), (t.p99_ms, 0.99), (t.max_ms, 1.0)] {
            assert!((got - want).abs() <= want / 16.0, "{got} vs {want}");
        }
    }
}
