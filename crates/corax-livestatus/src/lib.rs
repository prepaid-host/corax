//! Livestatus protocol server for corax.
//!
//! Implements the Livestatus query language (tables, columns, filters, stats, output
//! formats, `fixed16` response headers, keep-alive, `AuthUser`, `Wait*` headers and
//! `COMMAND`) on top of the corax in-memory [`corax_core::World`]. The table schema
//! is identical to the original Livestatus shipped with Checkmk, so the Checkmk GUI
//! and other Livestatus clients work unchanged.
//!
//! ```no_run
//! # async fn demo(shared: std::sync::Arc<corax_core::Shared>) -> anyhow::Result<()> {
//! corax_livestatus::serve(shared, "/omd/sites/mysite/tmp/run/live".into()).await
//! # }
//! ```

mod commands;
mod exec;
mod filter;
mod labels;
mod query;
mod render;
mod schema;
mod server;
mod tables;
mod value;

use std::sync::atomic::{AtomicU64, Ordering};

pub use exec::{execute, execute_async, Response};
pub use server::{answer, serve};
pub use value::{ColumnType, Value};

/// Livestatus server counters (exposed in the `status` table).
#[derive(Debug, Default)]
pub struct Stats {
    queries: AtomicU64,
    connections: AtomicU64,
    active: AtomicU64,
}

impl Stats {
    pub fn queries(&self) -> u64 {
        self.queries.load(Ordering::Relaxed)
    }

    pub fn connections(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }

    pub fn active_connections(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    pub(crate) fn count_query(&self) {
        self.queries.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn connection_opened(&self) {
        self.connections.fetch_add(1, Ordering::Relaxed);
        self.active.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn connection_closed(&self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Process-wide Livestatus counters.
pub fn stats() -> &'static Stats {
    static STATS: Stats =
        Stats { queries: AtomicU64::new(0), connections: AtomicU64::new(0), active: AtomicU64::new(0) };
    &STATS
}
