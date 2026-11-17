---
description: "Close the database pool when the listener ends without a shutdown signal, and correct the encode_query and run_scan doc comments in web.rs."
scope: task-viewer/src
created: 2026-11-13T20:45:23Z
updated: 2026-11-14T23:52:55Z
status: ready
reported-by: Andreas Hoffmann
---

# Close the pool on an early listener end and correct two doc comments

## Goal

A `serve` call whose listener ends before any shutdown signal arrives closes the database pool inside the same shutdown grace period a signalled stop uses, so an ended server leaves no open handle on `~/.task-viewer`. The `fn encode_query` and `fn run_scan` doc comments in `task-viewer/src/web.rs` state what their code does.

## Context

- Build order: independent. Its two doc-comment corrections were split out of `task-viewer_web-outcome-plumbing.md`, which needs no order against this task.
- `task-viewer/src/web.rs`'s `pub async fn serve` selects between a fired shutdown future and the `axum::serve` future ending on its own. The second arm logs `the listener stopped` and returns `Instant::now()` before the tail runs, and that tail holds the only `state.pool.close()` call, so the pool stays open on that path.
- The tail guards the close with `if !left.is_zero()`, so the early arm's `Instant::now()` is also a spent deadline: falling through with that value would skip the close as well. The arm needs its own grace budget.
- `fn encode_query`'s doc comment says a space becomes `%20` and every other byte outside the unreserved set becomes `%XX`. The keep-set match is `b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_'`, which omits `~`, an RFC 3986 unreserved character, so `~` is written `%7E`; `web_encode_query_matches_the_query_transport` pins that with `assert_eq!(encode_query("a-b_c.d~e"), "a-b_c.d%7Ee");`.
- `fn run_scan`'s doc comment says `Only this task acquires the lock; scan_core never does`. `fn add_repository` and `fn remove_repository` each take `state.scan` too, so the sentence is false about the module and true only about the scan-path functions.
- `ARCHITECTURE.md`'s **Load-Bearing Decisions** shutdown bullet describes the SIGTERM and Ctrl-C path, which already closes the pool and is covered by `web_serve_stops_accepting_while_the_drain_waits`; the early-end path is the one with neither a close nor coverage.
- This task was split out of [task-viewer_web-outcome-plumbing.md](archive/task-viewer_web-outcome-plumbing.md) by the user's fork ruling. It is the sole owner of the three items: the parent already carries only its `**Out of scope:**` pointer to this task, with no Goal sentence or Approach bullet for them, so the split is complete. Both files edit `task-viewer/src/web.rs` on disjoint regions, the parent's being the scan-outcome and banner paths and this task's `fn serve` and two doc comments, so neither orders the other.

## Approach

- Give the early-listener-end arm the same grace budget a signal gets: record the deadline as `Instant::now() + grace` and continue into the shared close instead of returning, so the close is not skipped by a spent deadline.
- Skip the drain on that arm and run the close. The drain awaits `&mut server`, which is already resolved there, so keep one `state.pool.close()` call site by extracting the timeout-and-warn close into a helper both arms call rather than duplicating it; the existing warn `the database close did not finish inside the shutdown grace period` stays.
- Make the arm reachable from an inline test without a signal: split `pub async fn serve` into a thin wrapper that builds `axum::serve(listener, app).with_graceful_shutdown(shutdown)` and an inner function that owns the select, the drain, and the close, taking the server future as a parameter and pinning it inside. `main.rs`'s call shape is unchanged.
- Rewrite the `fn encode_query` comment in place: a space becomes `%20`, and every other byte outside the keep-set `A-Z a-z 0-9 - . _` becomes `%XX` from the uppercase hex table, so `~` is written `%7E`.
- Rewrite the `fn run_scan` comment in place: the lock is held across the whole list-then-harvest-then-store sequence, `fn add_repository` and `fn remove_repository` take the same lock, and `scan_core` never takes it.

**Out of scope:** No change to `fn encode_query`'s keep-set or to `fn run_scan`'s lock discipline; both comments move to match the shipped code.

## Acceptance

- An inline test in `task-viewer/src/web.rs` hands the inner serve function a server future that resolves with an `io::Error` and no shutdown signal, and finds `state.pool.is_closed()` once it returns.
- That same forced early end returns a deadline with `remaining(deadline) > Duration::ZERO`, so the close runs on a fresh grace rather than a spent one, asserted in the same inline test.
- `fn serve`'s doc comment states that a listener that ends before a signal also closes the pool within `grace`, so its current silence about that path is superseded in `task-viewer/src/web.rs`.
- The `fn encode_query` doc comment in `task-viewer/src/web.rs` names the keep-set `A-Z a-z 0-9 - . _` and states that `~` is written `%7E`, and the sentence claiming every byte outside the unreserved set is encoded is gone.
- The `fn run_scan` doc comment in `task-viewer/src/web.rs` names `fn add_repository` and `fn remove_repository` as also taking the scan lock, and the sentence `Only this task acquires the lock` is gone.
