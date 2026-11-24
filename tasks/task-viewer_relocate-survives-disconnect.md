---
description: "Run Relocate's path write and re-harvest in a detached task that keeps the scan lock, so a dropped request still stores the new folder's rows and a fresh scan stamp."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-24T22:41:46Z
status: ready
reported-by: Andreas Hoffmann
---

# Relocate survives a dropped request

## Goal

A Relocate whose request disconnects mid-harvest still stores the new folder's rows and a fresh scan stamp. The repository never ends up on the new path with the previous folder's counts, the previous stamp, and no error.

## Context

`relocate_repository` in `task-viewer/src/web.rs` takes the scan lock inside the handler, commits the path, then awaits the re-harvest:

```rust
let _guard = state.scan.lock().await;
...
match db::update_repository_path(&state.pool, id, &path_text).await {
...
match refresh_one(&state.pool, &state.store_hook, id, &canonical).await {
```

Axum drops a handler future when the client disconnects, so the path write at `update_repository_path` is already committed while `refresh_one` never reaches `replace_tasks`. `update_repository_path` writes only `path`, leaving `scanned_at` and `last_error` untouched, so the Repositories page shows the new path with the old folder's `N tasks · M live` counts and the old stamp, and the dashboard charts the old folder's rows under the new path until the next Update or Rescan.

Add already solves the same problem by detaching the store:

```rust
let store = tokio::spawn(async move {
    let _guard = store_state.scan.lock().await;
    store_added(&store_state, &store_path).await
});
```

`FEATURES.md` section 3 promises that a relocated repository's rows "are re-harvested from the new folder under the scan lock so the row carries a fresh scan stamp". The repo's own tests model a disconnect by dropping the request future, and the store-pause hook (`TASK_VIEWER_TEST_STORE_PAUSE_MS` with `TASK_VIEWER_TEST_STORE_MARKER`) already gives a test a signal that a store is open inside its write transaction.

The lock must keep covering the `repository-path-taken` check as well as the write. Today the handler holds it across both, so two concurrent relocates to one path serialize and the second is refused with `repository-path-taken`. If the check stayed outside a detached write, the second request would pass it and then hit the `path` column's UNIQUE constraint, which reports `repository-save-failed` instead.

## Approach

1. Keep `choose_folder` in the handler, so a cancelled picker or a picker failure redirects before any task is spawned.
2. Move everything after the pick into one `tokio::spawn`: taking `state.scan`, the repository-list lookup, the not-found answer, the `repository-path-taken` check, `update_repository_path`, and `refresh_one`. The task holds the lock until the last row is stored and returns the banner its outcome names, including the `Ok(0)` not-found arm that today sits on the write.
3. Await only the join handle in the handler, and map a join error to the fixed save-failure banner, as Add does.
4. Update the sentences the change makes stale: `ARCHITECTURE.md`'s load-bearing decisions name Relocate as a detached store and add it to the shutdown grace list, `FEATURES.md` section 3 gains the same disconnect sentence Add already has, `TESTING.md`'s Repositories bullet records the disconnect case, and the `serve` and `relocate_repository` doc comments in `task-viewer/src/web.rs` name the detached store beside Add's.
5. Keep the `repository-path-taken` check inside the detached task's lock, so a second concurrent relocate to one path still waits and is refused with that code rather than hitting the `path` column's UNIQUE constraint and reporting a save failure.
6. Add a test that pauses a store through the store-pause hook, drops the relocate request future while the marker file exists, releases the pause, and then asserts the stored rows and scan stamp match the new folder. Add a second relocate test for an id that no longer exists, and one for a picker failure.

**Out of scope:**

- Detaching Rescan and the duplicate-clone confirm. A dropped Rescan leaves the old rows with the old stamp, which is consistent; a dropped confirm stores nothing.
- Changing which banner a refusal names.

## Acceptance

- [ ] A test pauses the store, drops the Relocate request future while the store is open, releases it, and asserts the repository's rows and scan stamp match the new folder; it fails on the current code because the cancelled store rolls back the path's re-harvest.
- [ ] A relocate for an id that no longer exists reports `repository-not-found`, and a picker failure reports its fixed code, each with a test.
- [ ] `ARCHITECTURE.md`, `FEATURES.md`, `TESTING.md`, and the two `web.rs` doc comments describe Relocate as a detached store that survives a disconnect.
- [ ] The `repository-path-taken` refusal still runs under the scan lock, proven by a concurrent-relocate test that reports that code rather than a save failure.
- [ ] `make lint` and `make test` pass.
