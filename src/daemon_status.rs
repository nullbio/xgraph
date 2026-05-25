//! Cheap, lock-free daemon-state probes consumed by every MCP response.
//!
//! `DaemonStatus` is shared between `WorktreeOwner` (which mutates it) and
//! `WorktreeHandler` (which reads it once per request to attach metadata to
//! the response envelope). Both probes here run in constant time:
//!
//! - `is_reconcile_done` is an `AtomicBool` load.
//! - `is_path_pending` / `any_pending` are DashSet lookups (one shard hash).
//! - `rss_bytes` reads `/proc/self/statm` (a tiny ~30-byte virtual file) and
//!   parses the second column. No allocation iteration.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dashmap::DashSet;

/// Daemon-wide state visible to MCP handlers. Kept in an `Arc` and shared
/// between the writer-side owner thread and the read-side tokio handlers.
pub struct DaemonStatus {
    initial_reconcile_done: AtomicBool,
    pending_paths: DashSet<PathBuf>,
    /// Cached `rss_bytes` value updated by `refresh_rss`. Avoids hitting
    /// `/proc/self/statm` on every single MCP request when the answer is
    /// stale by at most one refresh cycle.
    cached_rss_bytes: AtomicU64,
}

impl DaemonStatus {
    pub fn new() -> Self {
        Self {
            initial_reconcile_done: AtomicBool::new(false),
            pending_paths: DashSet::new(),
            cached_rss_bytes: AtomicU64::new(0),
        }
    }

    pub fn mark_reconcile_done(&self) {
        self.initial_reconcile_done.store(true, Ordering::Release);
    }

    pub fn mark_reconcile_running(&self) {
        self.initial_reconcile_done.store(false, Ordering::Release);
        self.pending_paths.clear();
    }

    pub fn is_reconcile_done(&self) -> bool {
        self.initial_reconcile_done.load(Ordering::Acquire)
    }

    pub fn mark_pending(&self, path: &Path) {
        self.pending_paths.insert(path.to_path_buf());
    }

    pub fn unmark_pending(&self, path: &Path) {
        self.pending_paths.remove(path);
    }

    pub fn is_path_pending(&self, path: &Path) -> bool {
        self.pending_paths.contains(path)
    }

    pub fn any_pending(&self) -> bool {
        !self.pending_paths.is_empty()
    }

    pub fn pending_count(&self) -> usize {
        self.pending_paths.len()
    }

    /// Re-read `/proc/self/statm` and update the cached value. Called once
    /// per request from the handler; cheap (~1µs).
    pub fn refresh_rss(&self) {
        if let Some(rss) = read_proc_statm_rss_bytes() {
            self.cached_rss_bytes.store(rss, Ordering::Relaxed);
        }
    }

    pub fn rss_bytes(&self) -> u64 {
        self.cached_rss_bytes.load(Ordering::Relaxed)
    }
}

impl Default for DaemonStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// Read `/proc/self/statm` and return the resident set size in bytes.
/// Returns `None` if the file can't be read or has unexpected shape.
pub fn read_proc_statm_rss_bytes() -> Option<u64> {
    // /proc/self/statm columns (all in pages):
    //   size resident shared text lib data dt
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
    // Page size is conventionally 4096 on x86_64 Linux. We could read
    // `sysconf(_SC_PAGESIZE)` but it's a syscall per call and the value
    // never changes for a process — the constant suffices.
    const PAGE_BYTES: u64 = 4096;
    Some(resident_pages.saturating_mul(PAGE_BYTES))
}

/// Threshold above which the handler emits a `high_memory_usage` warning.
/// Hardcoded for now; promote to a daemon config when tuning needs it.
pub const RSS_WARNING_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_set_roundtrip() {
        let status = DaemonStatus::new();
        let p = PathBuf::from("foo.rs");
        assert!(!status.is_path_pending(&p));
        status.mark_pending(&p);
        assert!(status.is_path_pending(&p));
        assert!(status.any_pending());
        status.unmark_pending(&p);
        assert!(!status.is_path_pending(&p));
        assert!(!status.any_pending());
    }

    #[test]
    fn reconcile_flag_defaults_false_then_latches_true() {
        let status = DaemonStatus::new();
        assert!(!status.is_reconcile_done());
        status.mark_reconcile_done();
        assert!(status.is_reconcile_done());
    }

    #[test]
    fn refresh_rss_returns_nonzero_on_linux() {
        let status = DaemonStatus::new();
        status.refresh_rss();
        let rss = status.rss_bytes();
        // We're running inside this process so RSS must be > 0.
        assert!(rss > 0, "expected non-zero RSS, got {rss}");
    }
}
