---
description: "Serialize launcher invocations, compare canonical paths, guard every kill, and never report a stop that did not take."
scope: scripts
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T13:25:45Z
status: open
reported-by: Andreas Hoffmann
---

# Make the launcher's process handling race-free and truthful

## Goal

Two launcher invocations cannot race the PID file: one takes an exclusive lock for the whole check-and-write, and no invocation deletes a PID file it did not write. `is_ours` recognizes the binary however it was invoked, including by a relative path. The graceful kill is guarded by `is_ours` like the force kill. A stop re-tests the process after the force kill and reports a survivor non-zero. A start timeout terminates the live server it started, and the launcher does not inherit a stray `TASK_VIEWER_PORT`.

## Context

- `scripts/task-viewer.sh` writes `echo "$candidate" >"$PID_FILE"` with no lock around the check, compares `[ "$args" = "$BINARY" ]` in `is_ours`, runs `kill "$pid" 2>/dev/null || true` unguarded, prints `task-viewer stopped` after `kill -9` with no re-test, and starts the server with `nohup "$BINARY"` on the no-python3 path.
- `SECURITY.md` requires process checks before signalling.
- Prior art: [task-viewer_launcher-ps-portability.md](archive/task-viewer_launcher-ps-portability.md) fixed `ps` truncation; [task-viewer_desktop-launcher.md](archive/task-viewer_desktop-launcher.md) built the launcher.

## Approach

- Wrap the whole start/stop body in an exclusive lock (`flock` or a lock directory) and delete a PID file only when this invocation wrote it.
- Canonicalize both sides in `is_ours`: resolve the argv path with `pwd -P`-style resolution and compare against a canonicalized `$BINARY`.
- Guard the graceful `kill` with `is_ours` using the PID read once.
- After `kill -9`, re-test the PID and exit non-zero with a message when it survives.
- On a start timeout, send `TERM` to the process when `is_ours` says it is ours.
- Unset `TASK_VIEWER_PORT` in the launcher before starting the server.

## Acceptance

- A test that runs two `start` invocations concurrently results in one running server and one truthful outcome, and no invocation removes the other's PID file.
- `start` with a relative binary path followed by `stop` stops the same process.
- A stop whose process survives `kill -9` exits non-zero and does not print "stopped".
- A forced start timeout leaves no live server process.
- An exported `TASK_VIEWER_PORT` does not change the port the launcher's server binds.
- `make lint` passes.
