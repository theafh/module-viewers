//! Shared `#[cfg(test)]` fixtures for inline suites. Adds zero runtime code.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A per-test scratch dir under `base`, unique by name/pid/nanos/counter.
pub fn unique_temp_dir_in(base: &Path, name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock should be after unix epoch")
        .as_nanos();
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    base.join(format!(
        "task-viewer_{name}_{}_{}_{}",
        std::process::id(),
        nanos,
        n
    ))
}

/// A per-test scratch dir under the system temp dir.
pub fn unique_temp_dir(name: &str) -> PathBuf {
    unique_temp_dir_in(&std::env::temp_dir(), name)
}
