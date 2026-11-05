---
description: "Do not hold the scan lock while the native folder picker is open, so an unattended Add dialog cannot block Update."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Do not hold the scan lock while the folder picker is open

## Goal

An open Add repository dialog no longer blocks Update. The picker runs before the lock, and the lock covers only the duplicate check, insert, and initial refresh.

## Context

`add_repository` in `task-viewer/src/web.rs` takes `state.scan.lock()` before calling `pick_and_store`, which awaits `spawn_blocking(pick_folder)`. A user who leaves the native dialog open keeps the scan lock indefinitely, so Update waits and the server appears hung.

## Approach

Move `pick_folder` outside the lock, then acquire the lock for the canonicalize and tasks check, duplicate check, `unique_name`, insert, and `refresh_one` steps. Keep the current cancellation and error redirects. Add a test that holds the picker open and asserts a concurrent scan can acquire the lock and run.

**Out of scope:** Running multiple folder pickers at once; the lock still serializes the store phase.

## Acceptance

- A test starts Add with a picker that blocks, runs Update, and asserts Update completes before the picker is released.
- A test asserts Add still inserts the repository and refreshes it once after the picker returns.
- A test asserts a cancelled picker still redirects with the existing notice.
