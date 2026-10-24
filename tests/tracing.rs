//! Request/statement tracing: a span opened around a unit of work emits one structured `span`
//! line on close, with its name, id, parent (when nested) and duration. Tracing is off by
//! default, so opening a span then costs nothing and emits nothing. Statements run through a
//! `statement` span when tracing is on.
//!
//! All assertions are in one test: tracing is a process-wide toggle, so running them as separate
//! parallel tests would race on it.

use chronos::obs::{self, capture, span};
use chronos::sql::run;
use chronos::{Db, MAIN};

/// The `id=` value of a captured span line, if present.
fn id_of(line: &str) -> Option<&str> {
    line.split_whitespace().find_map(|f| f.strip_prefix("id="))
}
fn parent_of(line: &str) -> Option<&str> {
    line.split_whitespace().find_map(|f| f.strip_prefix("parent="))
}

#[test]
fn spans_trace_when_on_and_are_silent_when_off() {
    // off (the default): a span emits nothing
    obs::set_tracing(false);
    let ((), off) = capture(|| {
        let _s = span("op", &[("k", "v".into())]);
    });
    assert!(off.iter().all(|l| !l.contains("event=span")), "off: {off:?}");

    // on: the span closes with a line carrying its name, a duration and its fields
    obs::set_tracing(true);
    let ((), on) = capture(|| {
        let _s = span("op", &[("k", "v".into())]);
    });
    let line = on.iter().find(|l| l.contains("event=span")).expect("a span line");
    assert!(line.contains("name=op"), "{line}");
    assert!(line.contains("dur_ms="), "{line}");
    assert!(line.contains("k=v"), "{line}");

    // nested: the inner span closes first and names the outer as its parent; the outer has none
    let ((), nested) = capture(|| {
        let _outer = span("outer", &[]);
        {
            let _inner = span("inner", &[]);
        }
    });
    let inner = nested.iter().find(|l| l.contains("name=inner")).expect("inner");
    let outer = nested.iter().find(|l| l.contains("name=outer")).expect("outer");
    assert_eq!(parent_of(inner), id_of(outer), "inner's parent is the outer span");
    assert!(parent_of(outer).is_none(), "the outer span is top-level: {outer}");
    // inner closes before outer
    let (pi, po) = (nested.iter().position(|l| l == inner), nested.iter().position(|l| l == outer));
    assert!(pi < po, "inner should close before outer");

    // a statement runs through a `statement` span when tracing is on
    let db = Db::new();
    let ((), stmt) = capture(|| {
        run(&db, MAIN, "select 1", &[]).unwrap();
    });
    assert!(
        stmt.iter().any(|l| l.contains("event=span") && l.contains("name=statement") && l.contains("dur_ms=")),
        "a statement span: {stmt:?}"
    );

    obs::set_tracing(false);
}
