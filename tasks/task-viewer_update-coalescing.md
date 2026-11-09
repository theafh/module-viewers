---
description: "Let Update clicks that arrive while a scan is queued share that one follow-up scan, so repeated clicks run at most one extra scan instead of one each."
scope: task-viewer
created: 2026-11-08T13:30:10Z
updated: 2026-11-08T18:12:57Z
status: ready
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
2. In `scan_all`, take the slot's lock in a scoped block that ends before the request awaits its receiver: when the slot holds a receiver whose sender is still alive, clone that receiver; otherwise create a `watch` channel, store a clone of its receiver in the slot, and spawn the detached run with the sender. The guard drops when that block ends, so the detached run can empty the slot instead of blocking on it, and the request then waits on its own receiver. A slot whose sender is gone, because its run ended without sending, counts as empty, so a crashed run never blocks later clicks.
3. `run_scan` is the only scan-path function that acquires `state.scan`, while `add_repository` and `remove_repository` keep their own acquisitions, so the detached run calls it once and its lock acquisition is the wait for a running scan; immediately after taking the lock, `run_scan` empties the slot before it lists the repositories, so a later click starts a new follow-up rather than joining a scan that may already be reading files. It then lists, harvests, and stores as it does today, its doc comment gains a clause naming the slot clear, and the detached run sends the returned result through the channel.
4. Each request waits with `wait_for` until the value is `Some`, then builds its own response through `scan_response` or the error redirect for its own project filter. A sender dropped without a value maps to the `STOPPED_ERROR` redirect, as a `JoinError` does today. A request that disconnects drops only its receiver, so the run still completes and stores.
5. Rewrite the detached-scan bullet under **Load-Bearing Decisions** in `ARCHITECTURE.md` and the disconnect sentence of the **3. Repositories** Update bullet in `FEATURES.md` in place, so each rewritten sentence carries these statements and keeps every statement of the sentence it replaces: clicks arriving while a scan runs share one follow-up scan, which starts after the running scan finishes and reads the repositories only after it starts; a request that disconnects does not cancel the run, which keeps the scan lock and stores every harvested result; and, in the `FEATURES.md` sentence, the next Update or Remove waits for the running scan to finish instead of racing its harvests. The new sentence replaces the old one at the same site. Extend the **Scan** bullet of `TESTING.md` with the cases below.

## Acceptance

- A `web` test holds the first Update's store with the store hook, as `web_second_update_waits_for_the_detached_store` does, fires three more Updates, releases the store, and finds exactly two store entries in total and a successful redirect with a flash token for each of the three later requests.
- A `web` test holds the first Update's store with the store hook, as `web_second_update_waits_for_the_detached_store` does, fires three more Updates while it is held, writes a new task file into the repository's `tasks/` folder, releases the store, and finds exactly two store entries in total with the repository's stored rows holding the original task and the new file, so the one follow-up those three clicks shared harvested a file the first run never saw.
- A `web` test makes the running scan and the follow-up report different outcomes: it replaces `state.harvest` with a stub that returns `HarvestError::MissingTasks` on its first call and `parsed_task()` on every later call, and holds that first call while it fires three more Updates and confirms after a short wait that none of them has finished. The running scan reports the `MISSING_TASKS_ERROR` banner, the follow-up reports `Charts updated.`, and each of the three later requests' flash token resolves to `Charts updated.`, none to the running scan's error banner, and the stub counted exactly two harvest calls in total, so the three later clicks shared one follow-up instead of running one each.
- A `web` test fires an Update after the follow-up has taken `state.scan` and finds that it runs a scan of its own, three store entries in total.
- A `web` test drops every request that joined the follow-up and finds that the follow-up still enters its store.
- A `web` test makes the follow-up's run end without a result through a store hook that panics once, finds the joined requests redirected with `STOPPED_ERROR`, and finds that the next Update starts and completes a fresh scan.
- `web_second_update_waits_for_the_detached_store` and the other scan-cancellation tests pass unchanged, and `web_add_holds_the_scan_lock_across_the_store`, `web_remove_waits_for_the_running_scan`, `web_remove_waits_for_the_detached_scan`, and `web_remove_waits_out_a_held_store_write_lock` pass unchanged.
- `ARCHITECTURE.md`, `FEATURES.md`, and `TESTING.md` each describe the shared follow-up scan once, with no superseded wording beside it, and the `ARCHITECTURE.md` and `FEATURES.md` sentences that carry it state that a request that disconnects does not cancel the run and the run finishes storing, while the `FEATURES.md` sentence also states that the next Update or Remove waits for the running scan to finish instead of racing its harvests.
