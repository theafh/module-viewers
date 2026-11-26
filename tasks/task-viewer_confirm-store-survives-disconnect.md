---
description: "Run the duplicate-clone confirm store in a detached task that takes the scan lock, so a disconnect cannot leave a repository with no rows."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T23:31:41Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Keep the confirm store alive when the request disconnects

## Goal

Posting the duplicate-clone confirm and then disconnecting still inserts the repository and finishes its first harvest. The repository never appears with no rows, no scan stamp, and no stored error because the request was dropped mid-store.

## Context

`confirm_repository` in `task-viewer/src/web.rs` takes the scan lock and the held token in the handler, then awaits `store_repository` inline. Add and Relocate both spawn a task that takes the scan lock itself, so a dropped request leaves their store running; the confirm path does not. If the client goes away between the insert and the first harvest, the insert commits while the harvest's result is never stored: `refresh_one` runs the harvest on `spawn_blocking`, which keeps running after the handler future drops, so its rows are harvested and discarded. The repository is left with a null `scanned_at` and a null `last_error`, which the Repositories page shows as "not scanned yet" with no banner explaining it.

The archived Relocate task [`tasks/archive/task-viewer_relocate-survives-disconnect.md`](archive/task-viewer_relocate-survives-disconnect.md) deferred this work with the rationale "a dropped confirm stores nothing"; that rationale is wrong, because `insert_repository` commits the repository row before the store, so a dropped confirm stores a repository with no rows.

## Approach

1. Move the scan-lock acquisition, the held-token take, and `store_repository` into a spawned task, mirroring `add_repository`.
2. The handler awaits only the join handle to choose the redirect banner; a dropped request drops the join, not the task. A join error logs the stopped store task and redirects with `repository-save-failed`, as `add_repository` does.
3. Take the token inside the task so the take and the store both live in the task that survives a disconnect; a request dropped after the take then cannot consume the token without storing the held path.
4. Keep the not-found answer for an unknown or already-used token.
5. Add an in-process test that mirrors `web_add_store_survives_a_dropped_request` and `web_relocate_store_survives_a_dropped_request`: inject `state.store_hook` with an entered channel and a release oneshot, post the confirm, `drop_request` the request future once the store is entered, release the pause, then acquire `state.scan` — which the detached task holds until its last row is stored — and only then assert the repository and its rows exist and the next read does not report an unscanned repository.
6. Rewrite the passages the change makes stale in place: `ARCHITECTURE.md`'s load-bearing decisions, extending the closing sentence "The confirm POST takes the lock for the held store, so a move never races a scan or a Remove." to name Confirm as the fourth detached store beside Add and Relocate, and adding the confirm store to the shutdown-grace enumeration "an Update scan, Add's or Relocate's detached store, or a Rescan"; `FEATURES.md`'s duplicate-clone paragraph; `TESTING.md`'s "Repositories:" bullet with the confirm drop case; and the `store_repository`, `serve`, and `run_scan` doc comments in `task-viewer/src/web.rs`, each of which today names only Add's or Relocate's detached store.

**Out of scope:** changing how a duplicate is detected, and the Add and Relocate paths.

## Acceptance

- A confirm POST whose request disconnects during the store still inserts the repository and stores its first harvest; the test fails on the current code because the dropped handler's insert commits while its harvest result is never stored, leaving the repository with no rows and a null scan stamp.
- The repository reads with rows and a scan stamp, or with a stored scan error.
- An unknown or already-used token still reports `repository-not-found`.
- `ARCHITECTURE.md`, `FEATURES.md`, and `TESTING.md` describe the confirm store as a detached task, and no passage still says the confirm handler takes the lock for the held store.
- The new test, `make lint`, and `make test` pass.
