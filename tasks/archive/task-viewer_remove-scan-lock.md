---
description: "Serialize Remove with Update so deleting a repository cannot race a scan store and produce a foreign-key failure or a spurious scan error."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-05T14:49:21Z
status: finished
implemented-by: "Andreas Hoffmann"
design-extended: false
reported-by: "Andreas Hoffmann"
---

# Serialize Remove with Update

## Goal

Removing a repository while Update runs cannot produce a foreign-key failure, a spurious scan failure, or a delete that waits out the busy timeout.

## Context

`remove_repository` in `task-viewer/src/web.rs` calls `db::delete_repository` without taking `state.scan`. `scan_all` holds that mutex across `scan_core`, and `add_repository` takes it. A delete that lands between a harvest and its `replace_tasks` makes the insert violate `tasks.repository_id REFERENCES repositories(repository_id)`; the transaction rolls back and the dashboard names the removed repository as failed. A delete that arrives while a store holds the write lock waits up to the five-second busy timeout.

## Approach

Acquire `state.scan.lock().await` in `remove_repository` before the delete, matching `add_repository` and `scan_all`. Keep the existing redirect messages. Add a test that starts a scan with a blocking harvest, removes a repository in the middle, and asserts the delete and the scan both finish without a foreign-key error. Drive that race as: harvest signals entered → spawn Remove → release harvest → join both under bounded timeouts, on the same channel / `current_thread` pattern as `task-viewer_folder-picker-lock.md`. For the store-write-lock race, drive a scan path that holds `state.scan` across hooked `replace_tasks` (as `scan_all` does); inject an async hold inside `replace_tasks` that runs after the first write that takes the SQLite write lock (after `DELETE FROM tasks`, or an equivalent write in that transaction) and `await`s past the five-second busy timeout so the runtime can poll Remove, then drive: store signals entered → spawn Remove → after the five-second busy timeout elapses, release store → join both under bounded timeouts on the same channel / `current_thread` pattern. The hold is injected through a `StoreHookFn` seam held in `AppState` as `store_hook: Arc<StoreHookFn>`, where `StoreHookFn = dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync`, mirroring the `HarvestFn` and `PickerFn` aliases beside it: `db::replace_tasks` takes that hook as its fourth argument and `await`s it after the `DELETE FROM tasks` and before the inserts, `store_outcome` and `scan_core` thread `state.store_hook` down to it, `AppState::new` sets it to a hook that resolves at once so production keeps its current call shape, and the tests assign the private field on a state built by `AppState::new`.

**Out of scope:** Changing the scan lock into a read/write lock or a queue.

## Acceptance

- A test starts a scan with a blocking harvest, removes a repository in the middle, and asserts the repository is gone, no rows remain, and no scan failure names it.
- A test starts Remove while a scan holds the store write lock past the five-second busy timeout and asserts Remove completes with the existing notice and without SQLITE_BUSY.
- A test asserts a normal remove still redirects with the existing notice.
- The race tests fail against the current lock-free `remove_repository` and pass after the lock is added.
