---
description: "Serialize launcher invocations, compare canonical paths, guard every kill, and never report a stop that did not take."
scope: scripts
created: 2026-11-13T13:25:45Z
updated: 2026-11-14T23:52:55Z
status: ready
reported-by: Andreas Hoffmann
---

# Make the launcher's process handling race-free and truthful

## Goal

Two launcher invocations cannot race the PID file: the exclusive lock spans the whole command, and only the lock holder deletes a PID file, so a start never removes the file a concurrent start wrote. `is_ours` recognizes this checkout's binary when `ps` prints it as an absolute path, a path relative to the checkout, or a bare name on `PATH`, with trailing arguments allowed. The graceful kill is guarded by `is_ours` like the force kill. A stop re-tests the process after the force kill and reports a survivor non-zero. A start timeout terminates the live server it started, and the server binds the launcher's own `PORT` rather than an inherited `TASK_VIEWER_PORT`.

## Context

- Build order: second of the launcher pair. Ships after `task-viewer_launcher-build-truth.md`.
- `scripts/task-viewer.sh` writes `echo "$candidate" >"$PID_FILE"` with no lock around the check, compares the whole `ps -o args=` line to `$BINARY` in `is_ours` (so a relative path, a bare name, or a trailing argument reads as foreign), runs `kill "$pid" 2>/dev/null || true` unguarded, prints `task-viewer stopped` after `kill -9` with no re-test, and starts the server with `nohup "$BINARY"` on the no-python3 path. `is_running` and both `do_start` failure paths also delete the PID file, and `do_stop` deletes it after a stop.
- Shared surface and ordering: [task-viewer_launcher-build-truth.md](archive/task-viewer_launcher-build-truth.md) rewrites `do_start`, `do_toggle`, and the `start_detached` no-python3 branch of the same file and ships first, replacing that `nohup "$BINARY"` line with a `setsid`-or-refuse branch; this task takes the launcher as that task leaves it, re-reads the detach branch before quoting it, and keeps every guard here working under whatever detach it leaves.
- The launcher's `PORT=7880` reaches only its own probes and printed URLs: `port_pid` asks `lsof`/`ss` about `:"$PORT"` and `port_answers` opens `/dev/tcp/127.0.0.1/$PORT`, while the server binds `TASK_VIEWER_PORT` or `web::PORT` (`7880`) from `task-viewer/src/main.rs` and `start_detached` passes it neither an argument nor that variable. `TESTING.md` requires deterministic, isolated, parallel-safe tests, so the concurrency test needs a launcher copy that moves the server's bind port, not only the probes.
- `SECURITY.md` requires process checks before signalling.
- Prior art: [task-viewer_launcher-ps-portability.md](archive/task-viewer_launcher-ps-portability.md) fixed `ps` truncation; [task-viewer_desktop-launcher.md](archive/task-viewer_desktop-launcher.md) built the launcher.

## Approach

- Take the exclusive lock once, wrapping the whole `case` dispatch so every command — `start`, `stop`, `status`, `restart`, and `toggle` — runs under it, and release it from an `EXIT` trap. `status` and `restart` must be covered because `status` reaches `is_running`, which writes and deletes the PID file, and `restart` runs `do_stop` and `do_start` as one command. The helpers take no lock of their own, so `do_start`'s restart path (`do_stop` under `if is_running && is_outdated`) runs inside the lock already held instead of nesting on it.
- Build the lock from `mkdir`, which is atomic on macOS and Linux, record the holder's PID in a file inside the directory, and reclaim a directory whose recorded PID is not alive by renaming it to a unique name first, so exactly one waiter wins the rename and the losers retry the normal acquire; a check-then-delete reclaim lets two waiters both act on one stale observation and lets the second delete a fresh live lock. `flock` is absent on macOS and stays unused.
- Delete the PID file only from inside the lock: remove one whose PID is not this checkout's live server, and remove the one the stop just performed wrote. A second invocation waits for the lock and re-reads the PID file under it, so a start never removes the file a concurrent start wrote.
- In `is_ours`, compare the first whitespace-delimited token of the `ps` args to `$BINARY`: an absolute token compares as written, a token holding a `/` resolves against `$PROJECT_DIR`, and a bare token resolves with `command -v`; canonicalize `$PROJECT_DIR` once with `pwd -P` so a checkout reached through a symlink compares equal. A token that resolves to nothing is not ours.
- Resolve a relative token against `$PROJECT_DIR` because every launcher-started server inherits that working directory; a server started from another directory under a relative path stays unrecognized and is left alone.
- Guard the graceful `kill` with `is_ours` using the PID read once.
- After `kill -9`, re-test with `is_ours` and exit non-zero with a message when the process survives.
- On a start timeout, send `TERM` to the process when `is_ours` says it is ours.
- Unset any inherited `TASK_VIEWER_PORT` when the launcher starts and export the launcher's own `PORT` as `TASK_VIEWER_PORT` for the server, so `PORT` alone sets the bind port and an exported `TASK_VIEWER_PORT` changes nothing.
- Stage every launcher test on a copy of `scripts/task-viewer.sh` with `HOME` and `XDG_STATE_HOME` pointing at a per-test temp dir, so the PID, log, stamp, and lock files stay out of live directories; a test that only sources the copy stages it under that temp dir, as `task-viewer/src/launcher_tests.rs` already does.
- Stage the concurrency test, which starts a server, in `task-viewer/tests/` as a process-level test over a checkout-shaped temp root: a `scripts/task-viewer.sh` copy, `target/release/task-viewer` symlinked to `env!("CARGO_BIN_EXE_task-viewer")`, and no-op `cargo`, `open`, and `xdg-open` in `<temp home>/.cargo/bin`, which the launcher's PATH reaches first once `task-viewer_launcher-build-truth.md` lands, so `PROJECT_DIR` is that root, `build_binary` succeeds without a release build, `BINARY` names a real server, and a successful start opens no browser. Its copy's `PORT` line names a free loopback port, taken by binding port 0 and closing it, so the launcher's probes and the server's exported bind both land there and the test never binds 7880.

## Acceptance

- A test that runs two `start` invocations concurrently against one launcher copy whose `PORT` line names a free loopback port leaves one running server on that port, one invocation reporting a start and the other reporting an already-running server, and the loser leaves the winner's PID file in place; a `stop` under the lock then removes that file and leaves no live process.
- `is_ours` accepts a staged `ps` line printing the binary as `./target/release/task-viewer`, one printing the absolute binary followed by an argument, and one printing a bare `task-viewer` name resolved on `PATH`, and rejects a path outside the checkout.
- A stop aimed at a PID belonging to an unrelated program sends it no signal, so neither the graceful `kill` nor the `kill -9` fires.
- A lock directory whose recorded PID is not alive is reclaimed by the next invocation, which starts the server; two invocations racing on one stale lock leave exactly one running server.
- A `status` invocation run while another invocation holds the lock waits for the holder to release it before reading the PID file, so it never reads a concurrent start's half-written file.
- A stop whose process survives `kill -9` exits non-zero and does not print "stopped".
- A forced start timeout leaves no live server process.
- An exported `TASK_VIEWER_PORT` does not change the port the launcher's server binds.
