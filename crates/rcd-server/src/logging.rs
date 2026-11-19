//! Structured logging: event/outcome evidence streams (herdr pattern).
//!
//! Every operation logs a start line and a complete/fail line sharing the
//! same request id and a fixed field vocabulary, so debug logs double as
//! acceptance-test evidence:
//!
//! ```text
//! DEBUG admin.request.start   method=search request_id=1
//! INFO  admin.request.complete method=search outcome=ok request_id=1 duration_ms=42 hits=2
//! ```
//!
//! Vocabulary (keep stable — tests grep for these):
//! - `event`: dotted name, e.g. `admin.request.complete`, `index.progress`
//! - `outcome`: `ok` | `error` | `timeout` | `started`
//! - `subsystem`: `daemon` | `admin` | `index` | `search` | `socket`

use std::sync::atomic::{AtomicU64, Ordering};

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn next_request_id() -> u64 {
    REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Admin-plane request received.
pub fn admin_request_start(request_id: u64, method: &str) {
    tracing::debug!(
        event = "admin.request.start",
        subsystem = "admin",
        outcome = "started",
        request_id,
        method,
        "admin request received"
    );
}

/// Admin-plane request finished — outcome ok/error plus duration.
pub fn admin_request_complete(request_id: u64, method: &str, outcome: &'static str, duration_ms: u128) {
    // tracing macro levels must be literals — branch on the two cases.
    if outcome == "ok" {
        tracing::info!(
            event = "admin.request.complete",
            subsystem = "admin",
            outcome,
            request_id,
            method,
            duration_ms,
            "admin request finished"
        );
    } else {
        tracing::warn!(
            event = "admin.request.complete",
            subsystem = "admin",
            outcome,
            request_id,
            method,
            duration_ms,
            "admin request finished"
        );
    }
}

/// Daemon lifecycle: startup with pid (crash reconciliation anchor).
pub fn daemon_startup(socket: &str, pid: u32) {
    tracing::info!(
        event = "daemon.startup",
        subsystem = "daemon",
        outcome = "started",
        socket,
        pid,
        "daemon starting"
    );
}

pub fn daemon_ready(socket: &str) {
    tracing::info!(
        event = "daemon.ready",
        subsystem = "daemon",
        outcome = "ok",
        socket,
        "daemon ready"
    );
}

pub fn daemon_stopping() {
    tracing::info!(
        event = "daemon.shutdown",
        subsystem = "daemon",
        outcome = "completed",
        "daemon stopping"
    );
}

/// Search completed — the acceptance evidence line (mode + hits).
pub fn search_complete(mode: &str, hits: usize, top_score: f32, duration_ms: u128) {
    tracing::info!(
        event = "search.complete",
        subsystem = "search",
        outcome = "ok",
        mode,
        hits,
        top_score = format!("{top_score:.3}"),
        duration_ms,
        "search completed"
    );
}

/// Connection/socket lifecycle — evidence for daemon-mutex bugs.
pub fn socket_bound(name: &str) {
    tracing::info!(
        event = "socket.bind",
        subsystem = "socket",
        outcome = "ok",
        name,
        "daemon socket bound (single-instance lock held)"
    );
}

pub fn socket_bind_conflict(name: &str) {
    tracing::warn!(
        event = "socket.bind.conflict",
        subsystem = "socket",
        outcome = "error",
        name,
        "socket already bound — another daemon is running"
    );
}

pub fn daemon_spawned(pid: u32, config: Option<&std::path::Path>) {
    tracing::info!(
        event = "daemon.spawn",
        subsystem = "daemon",
        outcome = "ok",
        pid,
        config = config.map(|p| p.display().to_string()).unwrap_or_default(),
        "daemon spawned detached"
    );
}

/// Single-instance mutex was already held — the loser exits. Logged by BOTH
/// sides (the winner logs socket_bound) so mutex incidents always leave a
/// paired trail in the daemon log.
pub fn mutex_lost(port_dir: &str, holder: &str) {
    tracing::warn!(
        event = "daemon.mutex_lost",
        subsystem = "daemon",
        outcome = "error",
        port_dir,
        holder,
        "another daemon answered the probe; this process exits without binding"
    );
}

/// Port-directory provenance: which dir this daemon uses (mutex scope).
pub fn port_dir_selected(port_dir: &str) {
    tracing::info!(
        event = "daemon.port_dir",
        subsystem = "daemon",
        outcome = "ok",
        port_dir,
        "admin port directory selected (mutex scope)"
    );
}
