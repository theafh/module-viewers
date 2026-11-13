---
description: "Add the promised date, oversized-blob, and in-flight-signal tests, and harden the graceful-shutdown test against port races, panics, and tautologies."
scope: task-viewer/tests
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Close the promised test coverage gaps

## Goal

`TESTING.md`'s promised scenarios each have a test that would fail if the behavior regressed: a quoted date, a committed blob over 4 MiB end to end, and a signal delivered while a store is in flight. The graceful-shutdown test no longer races another process for its port, leaves no live server behind after a panic, cleans its scratch directory on every path, asserts against the documented grace window, and its failure messages state what actually happened.

## Context

- `TESTING.md` lists scenarios with no test or only a partial one.
- `task-viewer/tests/graceful_shutdown.rs` sends its signal before any scan exists, binds port 0 and drops the listener before the child binds, has no `Drop` guard, and measures elapsed time from a deadline captured after the signal.
- `task-viewer` declares no library target: `task-viewer/Cargo.toml` carries no `[lib]` and `task-viewer/src/main.rs` declares `mod web;`, so the process-level test reaches the binary only through `env!("CARGO_BIN_EXE_task-viewer")` and can neither name `web::SHUTDOWN_GRACE` nor install a `cfg(test)` hook in the child process.
- `task-viewer/src/launcher_tests.rs` uses `std::os::unix::fs::PermissionsExt` but its module declaration is gated only on `test`, so a non-unix `cargo test` fails to compile.
- Prior art: [task-viewer_graceful-shutdown.md](archive/task-viewer_graceful-shutdown.md) delivered the shutdown behavior; these are the test gaps that let it regress.

## Approach

- Add a test for a quoted date value, a committed blob over 4 MiB charted end to end, and a signal sent while a store is in flight with the stored rows asserted afterward.
- Give the shutdown test a deterministic store pause without `cfg(test)` code in the spawned binary: read a test-only `TASK_VIEWER_TEST_STORE_PAUSE_MS` override in `web::AppState::new` in `task-viewer/src/web.rs`, beside the existing `TASK_VIEWER_SCAN_WORKERS` read, where a whole number of milliseconds replaces the default `store_hook` with one that sleeps that long inside `db::replace_tasks`, so the test holds a store open across the signal. `AppState::store_hook` is private to the `web` module, so the override is read where that field is built rather than in `task-viewer/src/main.rs`, whose `RUST_LOG` and `TASK_VIEWER_PORT` reads stay the only environment reads there. Name the override in the environment paragraph of `ARCHITECTURE.md` and leave it unset everywhere else.
- Replace the free-port helper with a bind-retry loop, or read the actual port from the server's log line.
- Add a `Drop` guard that kills and waits for the child and removes the scratch directory.
- Assert the drain against the documented four-second grace, declared as a local `GRACE: Duration = Duration::from_secs(4)` in the test whose doc comment names `web::SHUTDOWN_GRACE` in `task-viewer/src/web.rs` as the single source it mirrors, since the binary exposes no library target to import it from.
- Correct the two failure messages that state the opposite of what happened, and the doc comment that claims "drains" with no assertion.
- Gate `launcher_tests` on `#[cfg(all(test, unix))]`.

## Acceptance

- Each of the three promised scenarios has a test that fails when the behavior is reverted, verified by temporarily reverting one.
- The in-flight test passes ten consecutive runs with no timing-based flake.
- A panic in the shutdown test leaves no `task-viewer` process running and no scratch directory behind.
- The elapsed assertion fails when the test's `GRACE` mirror and `web::SHUTDOWN_GRACE` disagree: with a store held in flight past the grace, the server exits no earlier than the mirrored grace and still inside the launcher's window, so lowering `web::SHUTDOWN_GRACE` alone fails the run.
- The port comes from a bind-retry loop or the child's own log line, so no listener is bound and dropped before the child binds, and two runs started together cannot take each other's port.
- The corrected failure messages name the actual condition.
- `cargo test` compiles on a non-unix target configuration (or the module is provably excluded there).
- `ARCHITECTURE.md` names the test-only store-pause override with its default of unset, beside `TASK_VIEWER_PORT`.
- `make lint` and `make test` pass.
