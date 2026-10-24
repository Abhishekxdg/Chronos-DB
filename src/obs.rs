//! Observability: one structured log line per event, as `key=value` pairs, so logs can be
//! grepped and parsed instead of being free-form `eprintln!` text. Lines go to stderr; a test (or
//! a caller that wants to inspect them) can capture the lines emitted on its own thread with
//! [`capture`], which is why the sink is thread-local.
//!
//! The first user is the slow-query log (see `sql`'s statement accounting): a statement that runs
//! at least as long as `log_min_duration_statement` is logged with its time, who ran it, the world
//! and the rows, as Postgres's slow-query logging does.

use std::cell::RefCell;

thread_local! {
    /// When `Some`, log lines emitted on this thread are pushed here instead of written to stderr.
    static CAPTURE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
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
