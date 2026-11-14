---
description: "Detach Add's first harvest, distinguish picker cancel from failure, keep picker paths byte-exact, and separate missing folders from other errors."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-14T23:09:37Z
status: open
reported-by: Andreas Hoffmann
---

# Make Add survive a disconnect and report real errors

## Goal

Add's first store completes after a client disconnect: the insert and the first harvest run in a detached task that takes the scan lock and holds it until the last row is stored, so a dropped request no longer cancels the store. A picker cancel is distinguishable from a picker failure on macOS and Linux, and a failure reports one fixed `picker-failed` banner code while the picker's own stderr goes to the server log. A non-UTF-8 picked path is refused with a clear message or stored byte-exact, never lossily trimmed into a path that fails every later Update. A missing folder reports "no tasks directory" while a permission or metadata error reports its own cause. The blocking filesystem calls run off the async worker.

## Context

- `task-viewer/src/web.rs` runs Add's first harvest inside the request future, maps every non-zero zenity exit to `Ok(None)`, matches the macOS cancel on the English word "cancel" and drops stderr, decodes the picker output lossily and trims it, and maps every `canonicalize` or `metadata` error to "That folder has no tasks directory."
- The live task [task-viewer_web-outcome-plumbing.md](task-viewer_web-outcome-plumbing.md) lands first and closes every `/repositories` banner to a fixed code; this task then adds the one code `picker-failed` to that set, so one canonical banner rule holds.
- The finished task [task-viewer_folder-picker-lock.md](archive/task-viewer_folder-picker-lock.md) moved the picker off the scan lock; these are the remaining failure and disconnect paths.

## Approach

- Spawn Add's whole store — the insert in `store_repository` and its `refresh_one` harvest — as a `tokio::spawn` task that takes `state.scan` itself, and await its `JoinHandle` only to build the redirect, so a dropped request future drops the handle while the store keeps the lock and finishes. Update `web_add_holds_the_scan_lock_across_the_store` to assert the rows survive a dropped request future.
- Factor each picker's outcome into a pure function of its exit status and its stdout and stderr bytes, so the cancel rule is proven without a display.
- Return `Ok(None)` only for the cancel: zenity exit status 1 with empty stderr, and osascript exit status 1 whose stderr carries the AppleScript error number `-128` rather than the localized word "cancel". Any other non-zero exit is a failure.
- Report a picker failure with the fixed `picker-failed` banner code and write the picker's stderr to the server log with `tracing::error!`, so the redirect carries no free text.
- Keep the picker output as raw bytes; if it is not UTF-8, refuse it with the same `picker-failed` code and log the offending path, rather than trimming a lossy string.
- Match `NotFound` from `canonicalize` and the `tasks` probe separately from permission and metadata errors: report "no tasks directory" only for `NotFound`, and log and report the real error otherwise.
- Move `std::fs::canonicalize` and the `tasks` probe into `spawn_blocking`.

## Acceptance

- A test that drops the Add request future finds the repository inserted and its rows stored once the detached store finishes, and the scan lock free afterwards.
- A picker-outcome test over zenity's shape: exit status 1 with empty stderr is a cancel, while exit status 1 carrying "Failed to open display" on stderr is a failure that returns that stderr.
- A picker-outcome test over osascript's shape: exit status 1 with `-128` on stderr is a cancel, while exit status 1 with any other stderr is a failure.
- A tracing subscriber that captures the failure path records one log line carrying the picker's stderr before the redirect.
- A redirected picker failure carries `picker-failed` alone, and the `/repositories` page renders its text from the closed code set, so no picker stderr reaches the query.
- A picker returning a non-UTF-8 path either stores it byte-exact or refuses it with a message; a later Update does not fail on it.
- A folder that exists but cannot be read reports the permission error, and a missing folder reports "no tasks directory".
- The async handler performs no blocking filesystem call, verified by a `spawn_blocking` spy or a code-level assertion.
