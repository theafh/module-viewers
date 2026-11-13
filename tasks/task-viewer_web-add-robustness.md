---
description: "Detach Add's first harvest, distinguish picker cancel from failure, keep picker paths byte-exact, and separate missing folders from other errors."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T13:25:45Z
status: open
reported-by: Andreas Hoffmann
---

# Make Add survive a disconnect and report real errors

## Goal

Add runs its first harvest as a detached task, so a client disconnect releases the scan lock and completes the store instead of dropping it. A picker failure is distinguishable from a user cancel on every platform. A non-UTF-8 picked path is refused with a clear message or stored byte-exact, never lossily trimmed into a path that fails every later Update. A missing folder reports "no tasks directory" while a permission or metadata error reports its own cause. The blocking filesystem calls run off the async worker.

## Context

- `task-viewer/src/web.rs` runs Add's first harvest inside the request future, maps every non-zero zenity exit to `Ok(None)`, matches the macOS cancel on the English word "cancel" and drops stderr, decodes the picker output lossily and trims it, and maps every `canonicalize` or `metadata` error to "That folder has no tasks directory."
- The finished task [task-viewer_folder-picker-lock.md](archive/task-viewer_folder-picker-lock.md) moved the picker off the scan lock; these are the remaining failure and disconnect paths.

## Approach

- Spawn Add's first harvest as a detached task the way `scan_all` does, and update `web_add_holds_the_scan_lock_across_the_store` to assert the store survives a dropped request future.
- Return `Ok(None)` only for the documented cancel exit, and surface any other non-zero exit with its stderr.
- Detect the macOS cancel by exit status and a non-English-safe signal rather than the word "cancel".
- Keep the picker output as raw bytes; if it is not UTF-8, refuse it with a message naming the path, rather than trimming a lossy string.
- Match `NotFound` separately from permission and metadata errors, and log the real error.
- Move `std::fs::canonicalize` and the `tasks` probe into `spawn_blocking`.

## Acceptance

- A test that drops the Add request future still finds the harvested rows stored and the scan lock released.
- A zenity invocation that exits non-zero for a missing display reports a failure, not a cancel.
- A picker returning a non-UTF-8 path either stores it byte-exact or refuses it with a message; a later Update does not fail on it.
- A folder that exists but cannot be read reports the permission error, and a missing folder reports "no tasks directory".
- The async handler performs no blocking filesystem call, verified by a `spawn_blocking` spy or a code-level assertion.
- `make lint` and `make test` pass.
