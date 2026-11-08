---
description: "On SIGTERM or SIGINT stop accepting, give a running Update up to 4 s to store, close the database, and exit inside the launcher's stop window; document all four launcher state files."
scope: task-viewer
created: 2026-11-08T14:13:24
updated: 2026-11-08T14:13:24
status: open
reported-by: "Andreas Hoffmann"
---

# Shut down on SIGTERM without cutting off a running Update

## Goal

When the launcher stops the server, or a person presses Ctrl-C under `make run`, the server stops accepting connections, lets an Update that is already storing finish within a bounded grace period, closes the database, and exits on its own before the launcher's forced kill. An Update that completes inside the grace period keeps every row it harvested, where today the process dies at once and discards them. The launcher's state-file documentation names every file the launcher writes.

## Context

`main` in `task-viewer/src/main.rs` runs under `#[tokio::main]`, builds the `AppState`, and awaits `axum::serve(listener, app)` with no shutdown future and no signal handler, so SIGTERM ends the process immediately. The stop path in `scripts/task-viewer.sh` sends `kill`, polls `is_running` ten times at 0.5-second intervals, and then sends `kill -9`, so the server has five seconds to exit by itself.

Update runs inside a detached task that holds `state.scan` from listing the repositories through the last store, as `ARCHITECTURE.md` describes under **Load-Bearing Decisions**. Harvests run on blocking threads and can each hold a git child for up to the git deadline. Returning from an async `main` drops the tokio runtime, and that drop waits for every blocking task without a bound, so a plain graceful return could outlast the launcher's window and still end in `kill -9`. A store already interrupted is safe today: each repository's `replace_tasks` is one transaction, and WAL rolls an unfinished one back on the next open.

`tokio::signal` needs tokio's `signal` feature, which the current `features = ["full"]` includes. The archived, deferred `task-viewer_build-efficiency` plan lists a trimmed tokio feature set without `signal`, so reviving that plan after this task lands has to keep `signal`.

`SECURITY.md` under **Data** says the launcher keeps "its PID file and the server log" in its per-user state directory and that "both files are mode `600`", and `README.md` describes the same two files. The launcher defines four files there (`PID_FILE`, `LOG_FILE`, `BUILD_LOG`, and `STAMP_FILE`), creates them under `umask 077`, and repairs older modes with a `chmod 600` line that lists the first three and leaves out `STAMP_FILE`.

## Approach

1. Add a function in `task-viewer/src/web.rs` that serves a router until a given shutdown future resolves and then drains within a given grace period. It stops accepting through `axum::serve(..).with_graceful_shutdown(..)`, bounds the wait for open connections and for `state.scan` together by that grace period, and then closes `state.pool`. Production passes a grace period of 4 seconds, one second inside the launcher's window.
2. In `main.rs`, build the tokio runtime explicitly in place of `#[tokio::main]`, run that function with a shutdown future that resolves on SIGTERM (through `tokio::signal::unix` on Unix) or on Ctrl-C, log one line when the signal arrives, and end with `Runtime::shutdown_timeout` for the time left, so a blocking harvest cannot hold the process past the grace period.
3. Add `"$STAMP_FILE"` to the launcher's `chmod 600` repair line.
4. Rewrite the **Data** paragraph of `SECURITY.md` and the matching `README.md` sentence so each names the PID file, the server log, the build log, and the binary stamp, with all four at mode `600`. Add a **Load-Bearing Decisions** bullet to `ARCHITECTURE.md` stating the signal handling, the 4-second grace period, its relation to the launcher's 5-second stop window, and the dependence on tokio's `signal` feature.

**Out of scope:** Changing how the launcher starts or stops the server, which stays as the archived `task-viewer_launcher-ps-portability` task left it.

## Acceptance

- A `web` test runs the serve function with a oneshot as its shutdown future while a stand-in scan holds `state.scan`, fires the shutdown, releases the lock after a short delay inside the grace period, and finds that the function returns after the release and that the pool is closed.
- A `web` test holds `state.scan` past a short grace period passed to the same function and finds that the function returns within that grace period plus a small margin.
- `task-viewer/src/main.rs` contains a SIGTERM handler and a `shutdown_timeout` call, checked by reading it.
- `scripts/task-viewer.sh` repairs the mode of `STAMP_FILE` alongside the other three files.
- `SECURITY.md` and `README.md` each name all four launcher state files, and the "both files" wording is gone from both.
- `ARCHITECTURE.md` carries one **Load-Bearing Decisions** bullet naming the signals, the 4-second grace period, and the `signal` feature.
