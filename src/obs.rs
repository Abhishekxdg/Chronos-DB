//! Observability: one structured log line per event, as `key=value` pairs, so logs can be
//! grepped and parsed instead of being free-form `eprintln!` text. Lines go to stderr; a test (or
//! a caller that wants to inspect them) can capture the lines emitted on its own thread with
//! [`capture`], which is why the sink is thread-local.
//!
//! The first user is the slow-query log (see `sql`'s statement accounting): a statement that runs
//! at least as long as `log_min_duration_statement` is logged with its time, who ran it, the world
//! and the rows, as Postgres's slow-query logging does.

use std::cell::RefCell;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

thread_local! {
    /// When `Some`, log lines emitted on this thread are pushed here instead of written to stderr.
    static CAPTURE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    /// The ids of the spans open on this thread, innermost last, so a new span knows its parent.
    static SPANS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Whether request/statement tracing is on. Off unless turned on (see `tracing_enabled`).
static TRACING: AtomicBool = AtomicBool::new(false);
static TRACING_INIT: Once = Once::new();
/// The next span id; spans are numbered so a child can name its parent.
static NEXT_SPAN: AtomicU64 = AtomicU64::new(1);

/// Whether tracing is on: off unless `CHRONOS_TRACE` is set to something truthy, or `set_tracing`
/// turned it on. Read once from the environment, then only `set_tracing` changes it.
pub fn tracing_enabled() -> bool {
    TRACING_INIT.call_once(|| {
        let on = std::env::var("CHRONOS_TRACE")
            .map(|v| !matches!(v.trim(), "" | "0" | "off" | "false" | "no"))
            .unwrap_or(false);
        TRACING.store(on, Ordering::Relaxed);
    });
    TRACING.load(Ordering::Relaxed)
}

/// Turns tracing on or off (for embedding and tests); overrides the environment from now on.
pub fn set_tracing(on: bool) {
    TRACING_INIT.call_once(|| {}); // so the env can't flip it back later
    TRACING.store(on, Ordering::Relaxed);
}

/// A tracing span: opened around a unit of work and closed when it is dropped, when it emits one
/// `event=span` line with its name, id, parent (if nested) and duration, plus any fields given at
/// the start. When tracing is off, opening a span does nothing and costs an atomic load.
pub struct Span {
    id: Option<u64>,
    name: &'static str,
    parent: Option<u64>,
    start: Instant,
    fields: Vec<(String, String)>,
}

/// Opens a span named `name` with `fields`, if tracing is on (see `tracing_enabled`); otherwise a
/// no-op span that emits nothing. Nests under any span already open on this thread.
pub fn span(name: &'static str, fields: &[(&str, String)]) -> Span {
    if !tracing_enabled() {
        return Span { id: None, name, parent: None, start: Instant::now(), fields: Vec::new() };
    }
    let id = NEXT_SPAN.fetch_add(1, Ordering::Relaxed);
    let parent = SPANS.with(|s| s.borrow().last().copied());
    SPANS.with(|s| s.borrow_mut().push(id));
    Span {
        id: Some(id),
        name,
        parent,
        start: Instant::now(),
        fields: fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let Some(id) = self.id else { return };
        SPANS.with(|s| {
            let mut st = s.borrow_mut();
            if st.last() == Some(&id) {
                st.pop();
            }
        });
        let dur = self.start.elapsed().as_secs_f64() * 1000.0;
        let mut fields: Vec<(&str, String)> = vec![("name", self.name.to_string()), ("id", id.to_string())];
        if let Some(p) = self.parent {
            fields.push(("parent", p.to_string()));
        }
        fields.push(("dur_ms", format!("{dur:.3}")));
        for (k, v) in &self.fields {
            fields.push((k.as_str(), v.clone()));
        }
        log_line("INFO", "span", &fields);
    }
}

/// Emit one structured log line: `level=<level> event=<event>` followed by each field as
/// `key=value` (values with spaces or quotes are quoted). Captured on this thread if [`capture`]
/// is active, else written to stderr.
pub fn log_line(level: &str, event: &str, fields: &[(&str, String)]) {
    let mut line = format!("level={level} event={event}");
    for (k, v) in fields {
        line.push(' ');
        line.push_str(k);
        line.push('=');
        line.push_str(&quote(v));
    }
    let captured = CAPTURE.with(|c| match c.borrow_mut().as_mut() {
        Some(buf) => {
            buf.push(line.clone());
            true
        }
        None => false,
    });
    if !captured {
        eprintln!("{line}");
    }
}

/// A value for a `key=value` field: quoted (with inner quotes and backslashes escaped) when it is
/// empty or holds a space, a quote, or an equals sign, so each line parses back unambiguously.
fn quote(v: &str) -> String {
    if v.is_empty() || v.chars().any(|c| c.is_whitespace() || c == '"' || c == '=') {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        v.to_string()
    }
}

/// Run `f` with log lines emitted on this thread captured rather than printed; returns `f`'s value
/// and the lines, in order. Restores the previous sink afterward (captures can nest). For tests
/// and for a caller that wants to read the lines a piece of work produced.
pub fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    let prev = CAPTURE.with(|c| c.borrow_mut().replace(Vec::new()));
    let r = f();
    let lines = CAPTURE.with(|c| std::mem::replace(&mut *c.borrow_mut(), prev)).unwrap_or_default();
    (r, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_and_formats_lines() {
        let ((), lines) = capture(|| {
            log_line("WARN", "slow_query", &[("ms", "12.5".into()), ("sql", "select 1".into())]);
        });
        assert_eq!(lines, vec![r#"level=WARN event=slow_query ms=12.5 sql="select 1""#]);
    }

    #[test]
    fn capture_restores_the_previous_sink() {
        let ((), outer) = capture(|| {
            let ((), inner) = capture(|| log_line("INFO", "a", &[]));
            assert_eq!(inner, vec!["level=INFO event=a"]);
            log_line("INFO", "b", &[]);
        });
        assert_eq!(outer, vec!["level=INFO event=b"], "the inner capture doesn't swallow the outer's line");
    }
}
