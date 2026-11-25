---
description: "Make the launcher process tests clean up after a failed assertion and actually exercise the dead-PID reclaim branch."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T20:01:24Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Make the launcher process tests clean up and test the dead-PID reclaim

## Goal

The launcher process tests leave no detached server and no temp directory when an assertion fails, and the stale-lock case fails if the launcher's dead-PID liveness check is removed.

## Context

`task-viewer/tests/launcher_process_safety.rs` starts servers through the launcher's Python double-fork, so the server is a detached grandchild the test process cannot reap. Its `Root` helper has no `Drop`; every test cleans up with a trailing `fs::remove_dir_all` after its assertions, so a panic in an assertion skips the cleanup and leaves the server on the port and the temp directory on disk. `task-viewer/tests/graceful_shutdown.rs` guards the same risk with a `Drop` on its scratch and server types and asserts the cleanup on a panic.

The stale-lock case writes only the PID file:

```rust
fs::write(root.lock_dir().join("pid"), format!("{}\n", dead_pid()))
```

It writes no `cmd` file, so `acquire_lock` never enters the branch that reads `holder_cmd` and calls `kill -0`; the reclaim comes from the missing-command retry instead. Removing the `kill -0` check therefore leaves the case green, and no other case records a launcher-shaped `cmd` beside a dead PID, so the dead-PID arm stays unexercised.

## Approach

1. Add a `Drop` implementation to the `Root` helper that stops any server the case started and removes the temp directory, and route every case's cleanup through it.
2. In the stale-lock case, write a `cmd` line that names this launcher beside the dead PID, so the case reaches the `kill -0` branch and reclaims through it. Keep a second case or assertion for the missing-command retry.
3. Add a case whose cleanup is reached through a panicking assertion, or assert the `Drop` path directly, so the leak cannot return unnoticed.

**Out of scope:** changing the launcher's lock protocol, the stop window, or the server's shutdown drain.

## Acceptance

- A case that panics after starting a server leaves no listener on its port and no temp directory behind.
- Removing the `kill -0` check from `acquire_lock` makes the stale-lock case fail.
- The missing-command retry still has a case that passes without a `cmd` file.
- Every existing launcher process case keeps passing.
