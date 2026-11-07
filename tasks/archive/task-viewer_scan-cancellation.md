---
description: "Keep Update bounded and result-storing when the HTTP client disconnects, so a dropped request cannot release the scan lock while blocking harvests still run."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-07T08:31:15
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Keep Update bounded when the client disconnects

## Goal

Dropping the HTTP request for Update no longer releases the scan lock while blocking git harvests keep running, so the worker cap and the one-write-at-a-time rule still hold.

## Context

`scan_all` in `task-viewer/src/web.rs` holds the `scan` mutex and awaits `scan_core`, which spawns `spawn_blocking` harvests behind a semaphore and a `JoinSet`. If axum drops the handler, the guard releases and the `JoinSet` drop aborts the wrapper tasks, but `spawn_blocking` work cannot be aborted. A second Update can then start while the old git processes still run, and the old results are never stored.

## Approach

Run the scan in a spawned task that owns `state.scan` and the store loop, and await its handle from the handler. The handler takes no lock of its own; the spawned task acquires `state.scan` before it calls `db::list_repositories` and holds it until every harvested result is stored, so the whole list-then-harvest-then-store sequence stays under the one lock. The scan's lock ownership lives only in that spawned task — `scan_core` never acquires `state.scan`. If the request goes away, the task continues to completion and stores its results. `AppState` gains a `harvest: Arc<HarvestFn>` field mirroring `store_hook`: `AppState::new` in `task-viewer/src/web.rs` sets it to `Arc::new(harvest::harvest_repository_with_prior)`, and `scan_all` in `task-viewer/src/web.rs` passes `Arc::clone(&state.harvest)` to `scan_core` in place of the hardcoded harvest, so a test can install a harvest that signals its entry and blocks. Keep the existing report and redirect behavior for a normal request.

**Out of scope:** Per-repository cancellation; the scan either completes or is abandoned as a whole.

## Acceptance

- A test installs an `AppState.harvest` that records every entry and exit in a shared counter and blocks its first harvest on a release channel, drives two Updates through `router(state).oneshot(form_post("/scan", …))`, drops the first request future once that harvest signals its entry, and asserts that while the first harvest is held the second run's harvest never enters and that the peak number of concurrent blocking harvests across both runs never exceeds the cap; it then releases the held harvest and lets both runs finish.
- A test asserts the first scan still stores the rows it harvested after the request future is dropped.
- A test asserts a normal request still returns the same scan report.
- A test installs an `AppState.harvest` that signals its first entry and blocks on a release channel, starts Update through `router(state).oneshot(form_post("/scan", …))`, drops the request future once that harvest signals, then issues Remove for one of the listed repositories, and asserts Remove stays unfinished while the harvest is held and finishes after the release, once the detached scan stores its results and finishes.
- A test drives two Updates through `router(state).oneshot(form_post("/scan", None, None))` with a `StoreHookFn` that signals on its first entry and holds that entry open until a release channel fires, the way `web_remove_waits_out_a_held_store_write_lock` does: it drops the first request future once the hook reports that entry, starts the second Update, and asserts the second scan's hook entry and the second request's response both stay unobserved until the release fires; then it releases, waits for the second entry so the check is not vacuous, and asserts the counted peak of concurrently active hook entries stays at one.
