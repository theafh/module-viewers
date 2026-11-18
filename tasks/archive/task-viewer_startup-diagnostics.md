---
description: "Log after a successful bind, reject a malformed port, name the bind address, and stop treating a ctrl_c registration error as a signal."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-18T21:00:58Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: false
---

# Make start-up diagnostics truthful

## Goal

The "listening" line is logged only after the bind succeeds and names the bound address. A malformed or zero `TASK_VIEWER_PORT` fails with a message naming the value instead of silently binding 7880. A bind failure names the address it tried. A failed `ctrl_c` registration is not treated as a received signal. The default-path directory error names the variable that actually set the path.

## Context

- Build order: ships before `task-viewer_test-coverage-gaps.md`, whose shutdown test reads the post-bind listening line and relies on the zero-port rejection this task adds.
- `task-viewer/src/main.rs` logs `task-viewer listening on http://{addr}` before `TcpListener::bind`, parses the port with `raw.trim().parse().unwrap_or(web::PORT)`, and awaits `ctrl_c()` without distinguishing a registration error from a signal.
- `task-viewer/src/db.rs` builds the default-path directory error with a message that blames `TASK_VIEWER_DB` even when it is unset.
- Prior art: [task-viewer_utc-timestamps.md](task-viewer_utc-timestamps.md) covers the clock base; this task covers the start-up messages.

## Approach

- Bind first, then log the bound address from `listener.local_addr()`.
- Add a pure port-parse function that rejects a malformed value and zero, and have the caller fail start-up with a message naming the offending value, with no fallback to `web::PORT`.
- Attach the address as context to the bind error.
- Give `shutdown_signal` an injected `ctrl_c` future so a test can supply one. On a `ctrl_c()` registration error inside the `tokio::select!`, log the error and keep waiting for SIGTERM with `terminate.recv().await` instead of treating the error as a signal or returning, so the server stays up and SIGTERM still stops it. On the SIGTERM-handler-install failure path, await the same injected future, log a stop only when it resolves `Ok(())`, and on an error log it and stay up with `std::future::pending().await`, because no terminate handler exists to wait on.
- Pass a source label into the directory error so it names `TASK_VIEWER_DB` only when that variable set the path.

## Acceptance

- A second instance on an occupied port logs no "listening" line and exits with an error naming the address.
- A start-up helper that binds, reads `listener.local_addr()`, and emits the listening line has an inline test that binds `127.0.0.1:0`, captures the emitted line, and asserts it carries the ephemeral port `local_addr()` reported rather than `:0`, so the line is emitted only after a successful bind and names the bound address.
- `TASK_VIEWER_PORT=70000` and `TASK_VIEWER_PORT=abc` each fail naming the value; `TASK_VIEWER_PORT=0` is rejected rather than binding an ephemeral port.
- The pure port parser has inline tests for valid, zero, overflow, and non-numeric input.
- A new `#[cfg(test)] mod tests` in `main.rs` calls `shutdown_signal` with a `ctrl_c` future that resolves `Err` and asserts a `tokio::time::timeout` over it expires, so a registration error does not resolve it; the process-level `task_viewer_exits_on_sigterm_and_sigint` test in `task-viewer/tests/graceful_shutdown.rs` still proves a real SIGTERM stops the server.
- With `TASK_VIEWER_DB` unset, a default-path directory failure does not mention it.
