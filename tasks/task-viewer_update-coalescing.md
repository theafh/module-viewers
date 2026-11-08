---
description: "Let Update clicks that arrive while a scan is queued share that one follow-up scan, so repeated clicks run at most one extra scan instead of one each."
scope: task-viewer
created: 2026-11-08T14:30:10
updated: 2026-11-08T14:30:10
status: open
reported-by: "Andreas Hoffmann"
---

# Coalesce repeated Update clicks into one follow-up scan

## Goal

However many times a person clicks Update while a scan runs, at most one more scan follows it, and every click that arrived before that follow-up started reports the follow-up's outcome. Each reported outcome still reflects the files as they stood after that click, because a follow-up reads nothing before it starts. A single click, a click during a running scan, a disconnected request, and Remove behave as they do today. Repeated clicks stop multiplying the wait: today the last click's page waits for one full scan per earlier click.

## Context

`scan_all` in `task-viewer/src/web.rs` spawns `run_scan(state.clone())` for every Update request and awaits the handle, mapping a `JoinError` to the `STOPPED_ERROR` redirect. `run_scan` waits for `state.scan`, lists the repositories, and runs `scan_core`, which harvests and stores each repository. Because each request spawns its own detached run, N clicks during a slow scan queue N full scans behind the lock, and the request that renders last waits for all of them. `scan_response` turns a `ScanReport` into a flash banner and a redirect for the request's project filter. `ARCHITECTURE.md` describes the detached run under **Load-Bearing Decisions** ("Update owns the scan lock inside a detached task …"), and `FEATURES.md` under **3. Repositories** says the next Update "waits for it to finish instead of racing harvests that still run".

The follow-up scan keeps those promises: the click after a running scan still waits for it and then runs its own scan, so `web_second_update_waits_for_the_detached_store`, which checks that a second Update's store starts only after the first one's held store is released, stays valid. Only a third and later click changes, joining the waiting follow-up instead of queuing another scan.

## Approach

1. Add a pending-scan slot to `AppState`: a `tokio::sync::Mutex<Option<tokio::sync::watch::Receiver<Option<Arc<Result<ScanReport, &'static str>>>>>>`, initialised empty in `AppState::new` and in the test state builder.
2. In `scan_all`, lock the slot. When it holds a receiver whose sender is still alive, clone it and wait. Otherwise create a `watch` channel, store a receiver in the slot, and spawn the detached run with the sender. A slot whose sender is gone, because its run ended without sending, counts as empty, so a crashed run never blocks later clicks.
3. The detached run waits for `state.scan`, then empties the slot before it lists the repositories, so a later click starts a new follow-up rather than joining a scan that may already be reading files. It then runs the scan as `run_scan` does today and sends the result.
4. Each request waits with `wait_for` until the value is `Some`, then builds its own response through `scan_response` or the error redirect for its own project filter. A sender dropped without a value maps to the `STOPPED_ERROR` redirect, as a `JoinError` does today. A request that disconnects drops only its receiver, so the run still completes and stores.
5. Rewrite the detached-scan bullet under **Load-Bearing Decisions** in `ARCHITECTURE.md` and the disconnect sentence of the **3. Repositories** Update bullet in `FEATURES.md` so each states that clicks arriving while a scan runs share one follow-up scan, which starts after the running scan finishes and reads the repositories only after it starts. Extend the **Scan** bullet of `TESTING.md` with the cases below.

## Acceptance

- A `web` test holds the first Update's store with the store hook, as `web_second_update_waits_for_the_detached_store` does, fires three more Updates, releases the store, and finds exactly two store entries in total and a successful redirect with a flash token for each of the three later requests.
- A `web` test fires an Update after the follow-up has taken `state.scan` and finds that it runs a scan of its own, three store entries in total.
- A `web` test drops every request that joined the follow-up and finds that the follow-up still enters its store.
- A `web` test makes the follow-up's run end without a result through a store hook that panics once, finds the joined requests redirected with `STOPPED_ERROR`, and finds that the next Update starts and completes a fresh scan.
- `web_second_update_waits_for_the_detached_store` and the other scan-cancellation tests pass unchanged.
- `ARCHITECTURE.md`, `FEATURES.md`, and `TESTING.md` each describe the shared follow-up scan once, with no superseded wording beside it.
