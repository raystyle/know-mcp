//! Test observability helpers: phase-scoped time-budget assertions +
//! structured timing evidence.
//!
//! There is no single off-the-shelf crate for "budget assertion + phase
//! logs"; the established pattern is mature tools combined behind a thin
//! helper crate that every integration test imports. This is that helper.
//!
//! Usage:
//! ```no_run
//! use std::time::Duration;
//! use test_support::with_phase;
//!
//! #[test]
//! fn flow() {
//!     with_phase("setup", Duration::from_secs(10), || {
//!         // start services
//!     });
//!     with_phase("verify", Duration::from_secs(5), || {
//!         // assert
//!     });
//! }
//! ```
//! Log init: use `test-log` (`#[test_log::test]`) on the test fn — this
//! crate emits `tracing` events that test-log's subscriber captures.

use std::time::{Duration, Instant};

use tracing::{info, info_span};

/// Run `f` as a named phase; log elapsed_ms on completion and assert the
/// per-phase budget. Panicking here fails the test WITH the phase name and
/// measured time attached — far better evidence than a bare timeout.
pub fn with_phase<R>(name: &str, budget: Duration, f: impl FnOnce() -> R) -> R {
    let _span = info_span!("test_phase", phase = name).entered();
    let start = Instant::now();
    let result = f();
    assert_within(name, start, budget);
    result
}

/// Log elapsed and enforce the budget (shared tail of both variants).
fn assert_within(name: &str, start: Instant, budget: Duration) {
    let elapsed = start.elapsed();
    info!(
        phase = name,
        elapsed_ms = elapsed.as_millis() as u64,
        budget_ms = budget.as_millis() as u64,
        "phase completed"
    );
    println!(
        "[phase] {name}: {elapsed_ms}ms (budget {budget_ms}ms)",
        elapsed_ms = elapsed.as_millis(),
        budget_ms = budget.as_millis(),
    );
    assert!(
        elapsed <= budget,
        "phase '{name}' exceeded budget: {:?} > {budget:?} — treat as a hang/leak, not a slow pass",
        elapsed
    );
}

#[cfg(feature = "async")]
pub use async_helpers::*;

#[cfg(feature = "async")]
mod async_helpers {
    use super::assert_within;
    use std::time::{Duration, Instant};

    use tracing::{info_span, Instrument};

    /// Async variant of [`with_phase`] (feature "async").
    pub async fn with_phase_async<F, Fut, T>(name: &str, budget: Duration, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let span = info_span!("test_phase", phase = name);
        let start = Instant::now();
        let result = f().instrument(span).await;
        assert_within(name, start, budget);
        result
    }
}

/// Kill any stray `know-mcp` daemon left by a previously crashed test.
/// Leaked daemons hold the GPU + model and poison the global port file —
/// the root cause behind every "mystery 240s timeout" in this suite.
/// Also clears the global APPDATA port/lock files so the first probe
/// doesn't connect to a corpse.
pub fn kill_stray_daemons() {
    let _ = std::process::Command::new("taskkill")
        .args(["/IM", "know-mcp.exe", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let dir = std::path::PathBuf::from(appdata).join("know-mcp");
        let _ = std::fs::remove_file(dir.join("admin.port"));
        let _ = std::fs::remove_file(dir.join("daemon.lock"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_within_budget_passes() {
        let v = with_phase("fast", Duration::from_secs(1), || 42);
        assert_eq!(v, 42);
    }

    #[test]
    #[should_panic(expected = "exceeded budget")]
    fn phase_over_budget_panics() {
        with_phase("slow", Duration::from_millis(1), || {
            std::thread::sleep(Duration::from_millis(50));
        });
    }
}
