---
description: "Do not hold the scan lock while the native folder picker is open, so an unattended Add dialog cannot block Update."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T11:41:41
status: finished
implemented-by: "Andreas Hoffmann"
design-extended: false
reported-by: "Andreas Hoffmann"
---

# Do not hold the scan lock while the folder picker is open

## Goal

An open Add repository dialog no longer blocks Update. The picker runs before the lock, and the lock covers only the duplicate check, insert, and initial refresh.

## Context

`add_repository` in `task-viewer/src/web.rs` takes `state.scan.lock()` before calling `pick_and_store`, which awaits `spawn_blocking(pick_folder)`. A user who leaves the native dialog open keeps the scan lock indefinitely, so Update waits and the server appears hung.

## Approach

`add_repository` owns the lock and everything before it: it runs `pick_folder` through `state.picker`, `std::fs::canonicalize`, and the `tasks` directory check before it takes `state.scan.lock()`, then holds the lock while it calls the renamed `store_repository`, which runs the duplicate check — the `db::path_exists` path check, the `db::list_repositories` name read, and `db::unique_name` — then `db::insert_repository` and `refresh_one`. Cancellation is decided in `add_repository`, which now owns the picker. Keep the current cancellation and error redirects. Add a test that holds the picker open and asserts a concurrent scan can acquire the lock and run.

**Picker seam:** hold the picker in `AppState` as `picker: Arc<PickerFn>`, where `PickerFn = dyn Fn() -> Result<Option<PathBuf>, &'static str> + Send + Sync`, mirroring the existing `HarvestFn` alias beside it. `AppState::new` sets it to `pick_folder`, so `main.rs` keeps its current call unchanged, and `add_repository` runs a clone of `state.picker` through `spawn_blocking` before it takes the lock. The tests live in `web.rs`'s `mod tests`, which can assign the private field on a state built by `AppState::new`; the seam adds no test-only constructor and no new production entry point. The tests drive `add_repository`, not `store_repository`, so the handler's lock placement stays under test.

**Coordination:** this renames `pick_and_store` to `store_repository`, so `tasks/task-viewer_add-remove-outcomes.md` re-points its `refresh_one` return-value change at `store_repository`; `add_repository` still takes the lock, so `tasks/task-viewer_remove-scan-lock.md`'s `add_repository` anchor holds.

**Out of scope:** Running multiple folder pickers at once.

## Acceptance

- A test installs a blocking picker on the app state, starts Add through `add_repository`, and has the picker signal on a channel once it is entered; it awaits that signal under a bounded `tokio::time::timeout` (awaited asynchronously on the `current_thread` runtime) before it starts Update, then asserts Update completes inside a bounded `tokio::time::timeout` while the picker is still held, and releases the picker only after that assertion. The test fails against the current lock placement, where Add takes the lock before the picker, and passes once the picker runs before the lock.
- A test asserts Add still inserts the repository and refreshes it once after the picker returns.
- A test asserts a cancelled picker still redirects to bare `/repositories`, with no `notice` or `error` query parameter, matching today's `redirect_repositories("notice", "")` empty-message path.
