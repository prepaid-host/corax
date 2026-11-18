//! corax runtime: state model, scheduling, check execution and event processing.
//!
//! Architecture in one paragraph: all monitoring state lives in a single [`World`]
//! behind a `parking_lot::RwLock` inside [`Shared`]. Exactly one task — the
//! [`engine::Engine`] — mutates it. Everything else (check executors, the command
//! FIFO reader, the check result spool reader, Livestatus connections) talks to the
//! engine through the [`Input`] channel. Livestatus readers take short read locks.
//! Side effects produced while the lock is held (log lines, notifications,
//! perfdata, event handlers) are collected and executed after the lock is released.

pub mod checkresult;
pub mod checks;
pub mod cmdpipe;
pub mod commands;
pub mod downtime;
pub mod effects;
pub mod engine;
pub mod exec;
pub mod helper;
pub mod macros;
pub mod model;
pub mod nagioslog;
pub mod notify;
pub mod perfdata;
pub mod retention;
pub mod scheduler;
pub mod spool;
pub mod world;

#[cfg(test)]
pub(crate) mod testutil;

use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::{mpsc, watch};

pub use checkresult::CheckResult;
pub use model::*;
pub use world::{Paths, Settings, World};

/// Where an external command came from (only used for logging).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Pipe,
    Livestatus,
    Internal,
}

/// Messages processed by the engine.
#[derive(Debug)]
pub enum Input {
    /// A raw external command line, e.g. `[1700000000] SCHEDULE_FORCED_SVC_CHECK;h;s;1700000000`.
    /// The `[timestamp] ` prefix is optional.
    ExternalCommand { line: String, source: CommandSource },
    /// Result of an active or passive check.
    CheckResult(CheckResult),
}

/// State shared between the engine, Livestatus and I/O tasks.
pub struct Shared {
    pub world: RwLock<World>,
    paths: RwLock<Arc<Paths>>,
    input: mpsc::UnboundedSender<Input>,
    /// Incremented after every engine batch that changed state (Livestatus `WaitTrigger`).
    changes: watch::Sender<u64>,
}

impl Shared {
    pub fn new(world: World, paths: Paths) -> (Arc<Self>, mpsc::UnboundedReceiver<Input>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (changes, _) = watch::channel(0);
        let shared =
            Arc::new(Shared { world: RwLock::new(world), paths: RwLock::new(Arc::new(paths)), input: tx, changes });
        (shared, rx)
    }

    pub fn paths(&self) -> Arc<Paths> {
        self.paths.read().clone()
    }

    pub fn set_paths(&self, paths: Paths) {
        *self.paths.write() = Arc::new(paths);
    }

    /// Submit an external command (from Livestatus `COMMAND` or the command pipe).
    pub fn submit_command(&self, line: impl Into<String>, source: CommandSource) {
        let _ = self.input.send(Input::ExternalCommand { line: line.into(), source });
    }

    pub fn submit_result(&self, result: CheckResult) {
        let _ = self.input.send(Input::CheckResult(result));
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<Input> {
        self.input.clone()
    }

    /// Subscribe to state change notifications (value = change generation).
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(crate) fn notify_changed(&self) {
        self.changes.send_modify(|g| *g = g.wrapping_add(1));
    }
}

/// Current unix time in seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Current unix time as float seconds.
pub fn now_f64() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}
