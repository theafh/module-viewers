---
description: "Log after a successful bind, reject a malformed port, name the bind address, and stop treating a ctrl_c registration error as a signal."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Make start-up diagnostics truthful

## Goal

The "listening" line is logged only after the bind succeeds and names the bound address. A malformed or zero `TASK_VIEWER_PORT` fails with a message naming the value instead of silently binding 7880. A bind failure names the address it tried. A failed `ctrl_c` registration is not treated as a received signal. The default-path directory error names the variable that actually set the path.

## Context

- `task-viewer/src/main.rs` logs `task-viewer listening on http://{addr}` before `TcpListener::bind`, parses the port with `raw.trim().parse().unwrap_or(web::PORT)`, and awaits `ctrl_c()` without distinguishing a registration error from a signal.
- `task-viewer/src/db.rs` builds the default-path directory error with a message that blames `TASK_VIEWER_DB` even when it is unset.
- Prior art: [task-viewer_utc-timestamps.md](archive/task-viewer_utc-timestamps.md) covers the clock base; this task covers the start-up messages.

## Approach

- Bind first, then log `listener.local_addr()`.
- Add a pure port-parse function that rejects a malformed value and zero, and have the caller log or bail with the offending value.
- Attach the address as context to the bind error.
- On a `ctrl_c()` registration error, keep waiting for SIGTERM with `terminate.recv().await` instead of treating the error as a signal or returning, so the server stays up and SIGTERM still stops it.
- Pass a source label into the directory error so it names `TASK_VIEWER_DB` only when that variable set the path.

## Acceptance

- A second instance on an occupied port logs no "listening" line and exits with an error naming the address.
- `TASK_VIEWER_PORT=70000` and `TASK_VIEWER_PORT=abc` each fail naming the value; `TASK_VIEWER_PORT=0` is rejected rather than binding an ephemeral port.
- The pure port parser has inline tests for valid, zero, overflow, and non-numeric input.
- A simulated `ctrl_c` registration error leaves the server running rather than shutting down, and a SIGTERM sent afterwards still stops it.
- With `TASK_VIEWER_DB` unset, a default-path directory failure does not mention it.
- `make lint` and `make test` pass.
