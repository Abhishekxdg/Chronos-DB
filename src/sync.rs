//! Locks, atomics, channels and threads: the standard library's, or shuttle's under
//! `--features shuttle`, so the simulation tests (tests/sim.rs) control every interleaving and can
//! replay a failing one exactly. `Arc` and `Weak` aren't scheduling points, so they stay std.
//!
//! Only for state owned by a `Db`. A `static` must use std's types: it outlives one simulated run,
//! and `cargo test` runs simulations on several OS threads at once, which a shuttle atomic (a
//! `RefCell` underneath) doesn't survive: "RefCell already borrowed" at random.

#[cfg(feature = "shuttle")]
pub(crate) use shuttle::{
    sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, atomic, mpsc},
    thread,
};
pub(crate) use std::sync::{Arc, Weak};
#[cfg(not(feature = "shuttle"))]
pub(crate) use std::{
    sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, atomic, mpsc},
    thread,
};
