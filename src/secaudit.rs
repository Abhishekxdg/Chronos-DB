//! A security audit trail of privileged, security-relevant actions: agents and merge policies
//! created, changed or dropped, and system settings changed (ALTER SYSTEM). These are done by the
//! database's own user or an admin, so the per-world audit (`Core::audit`, reconstructed from the
//! log) doesn't show them; this records them directly.
//!
//! The trail is an in-memory ring of the newest `MAX` events — it covers this process's uptime,
//! not the whole history (so it isn't a durable record across restarts). Each event is also
//! written to the structured log (see `obs`), which is where a durable, shippable trail lives.
//! `SHOW SECURITY AUDIT` reads the ring, newest first.

use std::collections::VecDeque;
use std::sync::Mutex;

/// The most events kept; the oldest fall off.
const MAX: usize = 1000;

/// One privileged action: when, who did it (empty is the database's own user), what, and on what.
#[derive(Clone, Debug)]
pub struct Event {
    pub at: u64,
    pub actor: String,
    pub action: String,
    pub target: String,
}

/// The ring of recent security events (see the module docs).
#[derive(Default)]
pub struct Log(Mutex<VecDeque<Event>>);

impl Log {
    /// Records one action by `actor` (empty is the database's own user). Also emits a structured
    /// `security` log line, so the event survives in the log even though the ring is in memory.
    pub fn record(&self, actor: &str, action: &str, target: &str) {
        let who = if actor.is_empty() { "owner" } else { actor };
        crate::obs::log_line(
            "INFO",
            "security",
            &[("actor", who.into()), ("action", action.into()), ("target", target.into())],
        );
        let event =
            Event { at: crate::disk::now_ms(), actor: actor.into(), action: action.into(), target: target.into() };
        let mut q = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if q.len() >= MAX {
            q.pop_front();
        }
        q.push_back(event);
    }

    /// The most recent `limit` events, newest first.
    pub fn recent(&self, limit: usize) -> Vec<Event> {
        let q = self.0.lock().unwrap_or_else(|p| p.into_inner());
        q.iter().rev().take(limit).cloned().collect()
    }
}
