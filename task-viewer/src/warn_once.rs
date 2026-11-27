//! A gate for warnings that name one stored row or one offending value, so a
//! repeated dashboard read, JSON export, or CSV export logs each warning once
//! per process instead of once per render.
//!
//! The production gate is process-wide. A `#[cfg(test)]` build uses a
//! thread-local set instead, so every test starts with a fresh instance and
//! parallel tests never clear each other's keys, without any test opting in.

use std::collections::HashSet;

/// One warning event a stored row can carry. The label keeps two warnings about
/// one row from collapsing into each other, so a row whose `created` and ship
/// day both lie after today warns twice.
pub(crate) const CREATED_AFTER_TODAY: &str = "created-after-today";
pub(crate) const SHIPPED_AFTER_TODAY: &str = "shipped-after-today";
pub(crate) const DEFERRED_AFTER_TODAY: &str = "deferred-after-today";
pub(crate) const BEFORE_FLOOR: &str = "before-floor";
pub(crate) const UNPARSEABLE_UPDATED: &str = "unparseable-updated";
pub(crate) const UNPARSEABLE_KIND_OR_CREATED: &str = "unparseable-kind-or-created";

/// The identity of one warning: its event, the repository the row belongs to,
/// the row's stored path, and the offending value or date. Two warnings about
/// one row with different events or values stay distinct, while a repeated
/// render of the same warning supplies the same key.
type WarnKey = (String, i64, String, String);

fn key(event: &str, repository_id: i64, path: &str, value: &str) -> WarnKey {
    (
        event.to_string(),
        repository_id,
        path.to_string(),
        value.to_string(),
    )
}

/// Whether this key has not been seen in this process. Returns `true` exactly
/// once per distinct key, so the caller emits its warning only on that call.
#[cfg(not(test))]
pub(crate) fn first_sighting(event: &str, repository_id: i64, path: &str, value: &str) -> bool {
    use std::sync::{Mutex, OnceLock};

    static SEEN: OnceLock<Mutex<HashSet<WarnKey>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    seen.insert(key(event, repository_id, path, value))
}

// The test-build gate. Each test thread gets its own set, so a test that calls
// the read path twice still observes one warning while a parallel test's keys
// stay untouched.
#[cfg(test)]
thread_local! {
    static SEEN: std::cell::RefCell<HashSet<WarnKey>> =
        std::cell::RefCell::new(HashSet::new());
}

#[cfg(test)]
pub(crate) fn first_sighting(event: &str, repository_id: i64, path: &str, value: &str) -> bool {
    SEEN.with(|seen| {
        seen.borrow_mut()
            .insert(key(event, repository_id, path, value))
    })
}
