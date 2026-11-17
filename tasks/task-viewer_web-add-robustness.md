---
description: "Detach Add's first harvest, distinguish picker cancel from failure, keep picker paths byte-exact, and separate missing folders from other errors."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-15T07:35:53Z
status: ready
reported-by: Andreas Hoffmann
---

# Make Add survive a disconnect and report real errors

## Goal

Add's first store completes after a client disconnect: the insert and the first harvest run in a detached task that takes the scan lock and holds it until the last row is stored, so a dropped request no longer cancels the store. A picker cancel is distinguishable from a picker failure on macOS and Linux, and a failure reports one fixed `picker-failed` banner code while the picker's own stderr goes to the server log. A non-UTF-8 picked path is refused with a clear message, never lossily trimmed into a path that fails every later Update. A missing folder reports "no tasks directory" while a permission or metadata error reports its own cause. The blocking filesystem calls run off the async worker.

## Context

- Build order: second on the banner channel. Ships after `task-viewer_web-outcome-plumbing.md`, after `task-viewer_harvest-git-failure-cases.md`, which widens the same `add_repository` gate this task classifies, and after `task-viewer_web-pool-close-and-comments.md`, whose Acceptance pins the `fn run_scan` comment this task must refresh.
- `task-viewer/src/web.rs` runs Add's first harvest inside the request future, maps every non-zero zenity exit to `Ok(None)`, matches the macOS cancel on the English word "cancel" and drops stderr, decodes the picker output lossily and trims it, and maps every `canonicalize` failure and every `tasks` `is_dir` probe that returns false to "That folder has no tasks directory."
- The finished task [task-viewer_web-outcome-plumbing.md](archive/task-viewer_web-outcome-plumbing.md) landed first and closed every `/repositories` banner to a fixed code; this task then adds three codes to that set, `picker-failed`, `folder-unreadable`, and `path-not-utf8`, so one canonical banner rule holds.
- The finished task [task-viewer_folder-picker-lock.md](archive/task-viewer_folder-picker-lock.md) moved the picker off the scan lock; these are the remaining failure and disconnect paths.

## Approach

- Spawn Add's whole store — the insert in `store_repository` and its `refresh_one` harvest — as a `tokio::spawn` task that takes `state.scan` itself, and await its `JoinHandle` only to build the redirect, so a dropped request future drops the handle while the store keeps the lock and finishes. Update `web_add_holds_the_scan_lock_across_the_store` to assert the rows survive a dropped request future, renaming it to match its new assertion.
- Factor each picker's outcome into a pure function of its exit status and its stdout and stderr bytes, so the cancel rule is proven without a display.
- Return `Ok(None)` only for the cancel: zenity exit status 1 with empty stderr, and osascript exit status 1 whose stderr carries the AppleScript error number `-128` rather than the localized word "cancel". A zero exit with empty stdout is neither, so it reports `picker-failed` with empty stderr. Any other non-zero exit is a failure.
- Report a picker failure with the fixed `picker-failed` banner code and write the picker's stderr to the server log with `tracing::error!`, so the redirect carries no free text. Update `web_add_reports_a_picker_error`, which today asserts the free text `?error=No%20picker%20available.`, to the `picker-failed` code. Emit that log line in the async handler, not inside the `spawn_blocking` closure: the classifier returns the stderr, and the handler logs it, because a thread-scoped test subscriber cannot see a line emitted on a blocking worker.
- Keep the picker output as raw bytes; if it is not UTF-8, refuse it with its own `path-not-utf8` code and log the offending path, rather than trimming a lossy string. The code is distinct from `picker-failed` because the picker succeeded and the picked path is the problem. A successful pick is decoded with `std::str::from_utf8` and only its trailing newline stripped, every other byte preserved, so a path with spaces or non-ASCII characters stores exactly as picked and `canonicalize` still finds it.
- Match `NotFound` from `canonicalize` and the `tasks` probe separately from permission and metadata errors: report "no tasks directory" only when `canonicalize` returns `NotFound`, or when neither the working tree's `tasks/` directory exists nor the ref union lists a markdown path under `tasks/` — the same disjunction `task-viewer_harvest-git-failure-cases.md` defines, so a folder with an empty working-tree `tasks/` is still accepted; report the `folder-unreadable` code when `canonicalize` or the `std::fs::metadata(<picked>/tasks)` probe fails for any other reason, and log the real error. Run the probe before the ref-union check.
- Refresh the `fn run_scan` doc comment so it names the detached store task rather than `fn add_repository` as the Add path's scan-lock holder, and re-read `store_repository`'s "The caller holds the scan lock across this call" comment against the spawned caller.
- Move `std::fs::canonicalize` and the `tasks` probe into `spawn_blocking`.

## Acceptance

- A test that drops the Add request future finds the repository inserted and its rows stored once the detached store finishes, and the scan lock free afterwards. While the detached store runs, a second Add or an Update waits for it rather than proceeding, proved by parking the store on `store_hook` and observing the second request block until release.
- A picker-outcome test over zenity's shape: exit status 1 with empty stderr is a cancel, while exit status 1 carrying "Failed to open display" on stderr is a failure that returns that stderr.
- A picker-outcome test over osascript's shape: exit status 1 with `-128` on stderr is a cancel, while exit status 1 with any other stderr is a failure.
- A tracing subscriber that captures the failure path records one log line carrying the picker's stderr before the redirect.
- A redirected picker failure carries `picker-failed` alone, and the `/repositories` page renders its text from the closed code set, so no picker stderr reaches the query. `web_add_reports_a_picker_error` asserts that code and no picker text.
- The `fn run_scan` doc comment names the detached store task as the Add path's scan-lock holder, still names `fn remove_repository` as a lock taker, and no longer claims `fn add_repository` holds it.
- A successful pick whose stdout carries a trailing newline and a non-ASCII UTF-8 path stores the repository at exactly that path.
- A picker returning a non-UTF-8 path is refused with the `path-not-utf8` code, no repository row exists for that path, and a later Update does not fail on it.
- A folder that exists but cannot be read reports the `folder-unreadable` code and logs the real error, proved with a `chmod 000` on the picked repository folder whose mode is restored before cleanup, so `metadata` of `<picked>/tasks` returns `EACCES` while `canonicalize` of the picked path still succeeds, and a missing folder reports "no tasks directory".
- The async handler performs no blocking filesystem call, proved by a counter held on `AppState` (not a module static, so the test stays parallel-safe) that increments once per offloaded call and reads two after the redirect, so the item fails if either `canonicalize` or the `tasks` probe returns to the handler.
