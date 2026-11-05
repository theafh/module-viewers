---
description: "Keep Update bounded and result-storing when the HTTP client disconnects, so a dropped request cannot release the scan lock while blocking harvests still run."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Keep Update bounded when the client disconnects

## Goal

Dropping the HTTP request for Update no longer releases the scan lock while blocking git harvests keep running, so the worker cap and the one-write-at-a-time rule still hold.

## Context

`scan_all` in `task-viewer/src/web.rs` holds the `scan` mutex and awaits `scan_core`, which spawns `spawn_blocking` harvests behind a semaphore and a `JoinSet`. If axum drops the handler, the guard releases and the `JoinSet` drop aborts the wrapper tasks, but `spawn_blocking` work cannot be aborted. A second Update can then start while the old git processes still run, and the old results are never stored.

## Approach

Run the scan in a task that owns the lock and the store loop, and await its handle from the handler. If the request goes away, the task continues to completion and stores its results. Alternatively, add a cancellation token that the handler sets on drop and have `scan_core` stop spawning and wait for in-flight blocking work before releasing the lock. Keep the existing report and redirect behavior for a normal request.

**Out of scope:** Per-repository cancellation; the scan either completes or is abandoned as a whole.

## Acceptance

- A test starts `scan_core` through a future that is dropped after the first harvest starts, then starts a second scan, and asserts the number of concurrent blocking harvests never exceeds the cap.
- A test asserts the first scan still stores the rows it harvested after the request future is dropped.
- A test asserts a normal request still returns the same scan report.
