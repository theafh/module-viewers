---
description: "Make the foreign-PID stop test's liveness probe read the child's process state, so an exited or zombie child cannot print foreign-alive and pass the test for the wrong reason."
scope: task-viewer
created: 2026-11-27T21:24:30Z
updated: 2026-11-27T23:10:49Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Make the foreign-PID stop test's liveness probe reject a zombie child

## Goal

`launcher_stop_guards_each_kill_against_a_foreign_pid` in `task-viewer/src/launcher_tests.rs` reports `foreign-alive` only for a foreign child that is actually running, on Linux and macOS alike. A child that has exited, including the unreaped zombie whose `kill -0` still succeeds on Linux, reads as `foreign-dead` and fails the test. The empty kill-log assertion remains the load-bearing guard for the two `is_ours` kill guards.

## Context

The test's driver `REUSED_FOREIGN_PID_DRIVER` launches its own background `sleep 60`, records that PID in `$PID_FILE`, and after `do_stop` runs the liveness probe:

```bash
unset -f kill
if kill -0 "$foreign" 2>/dev/null; then
    echo "foreign-alive"
else
    echo "foreign-dead"
fi
```

`kill -0` answers whether a signal could be delivered, not whether the process still runs. On Linux it succeeds for a zombie, so a `sleep` that already exited but was not reaped still prints `foreign-alive`. That is exactly the failure the probe exists to catch: the driver launches the child before defining `sleep() { :; }` so the stub cannot turn the job into a no-op subshell, and the probe is meant to fail if that ordering is ever reversed. With `kill -0` it cannot.

The driver also defines a `ps()` shell function that answers only a `-ww` read, and prints nothing for any other argument list. A naive `ps -o stat= -p "$foreign"` therefore calls that stub and reads empty, which would report every child dead. The probe must reach the real `ps`, either with `command ps` or after `unset -f ps`.

The same file already carries the shape this task needs: `MOVED_STOP_DRIVER` defines an `alive()` helper that runs `command ps -p "$TARGET" -o stat=` and rejects an empty state or one beginning with `Z`. This task applies that shape to the foreign-PID driver's probe.

The test and its driver were created by the archived finished task [Test the stop guard against a foreign PID](task-viewer_launcher-foreign-pid-stop-test.md), whose acceptance already promised that the foreign process "stays alive"; this task makes that half of the test able to observe a dead child. A sibling task, [Stub `ps` in the launcher status exit-code test](task-viewer_launcher-status-exit-code-test-ps-stub.md) (archived), adds a `ps` stub to a different driver in the same file, so the two edits share the file and no driver.

`TESTING.md`'s `- Launcher process safety:` bullet states `a stop aimed at an unrelated PID sends it no signal`. This task changes only how the test observes that outcome, so the sentence stays accurate and unchanged.

## Approach

1. In `REUSED_FOREIGN_PID_DRIVER`, replace the `kill -0 "$foreign"` verdict with a state read that bypasses the driver's `ps()` stub: mirror `MOVED_STOP_DRIVER`'s `alive()` helper, running `command ps -p "$foreign" -o stat=`, and treat a non-zero exit, an empty state, and a state beginning with `Z` as dead. Keep `unset -f kill` before the probe so the trailing `kill -9 "$foreign"` cleanup reaches the real builtin instead of the logging stub that appends to `$KILL_LOG`, and keep the printed `foreign-alive` / `foreign-dead` tokens the Rust assertion reads.
2. Rewrite the comment above `unset -f kill` in place so it names every reason for the shape once: the unset keeps the trailing `kill -9 "$foreign"` cleanup off the logging stub, the probe reaches the real `ps` past the driver's `ps()` stub, and a zombie state is rejected because `kill -0` accepts it. Replace the current sentence that says the probe asks the real `kill`, rather than adding a second explanation beside it.
3. Leave `do_stop`, the driver's `ps()` stub, the stdout assertions, and the empty kill-log assertion untouched.
4. Prove the new probe with a staged fake `ps` earlier on `PATH` that prints `Z` for the state read: the test must fail on the `foreign-alive` assertion, and pass again once the fake `ps` is removed.

**Out of scope:** rewriting the closing `kill -0` probes in other drivers of the same file, such as `MOVED_STOP_DRIVER`'s `server-alive` / `server-dead` read; this task strengthens the foreign-PID driver's probe only.

## Acceptance

- In `REUSED_FOREIGN_PID_DRIVER`, the liveness verdict comes from `command ps -p "$foreign" -o stat=` and reports `foreign-alive` only for a non-empty state that does not begin with `Z`; the driver contains no `kill -0 "$foreign"` verdict.
- The probe bypasses the driver's `ps()` stub, proven by the state read naming `command ps` (or an `unset -f ps` before it) while the stub still answers only `-ww`.
- The comment above `unset -f kill` names the unset's reason — the trailing `kill -9 "$foreign"` cleanup must reach the real builtin rather than the logging stub — and names the probe's stub bypass and zombie rejection, with the old sentence that says the probe asks the real `kill` gone, leaving one canonical explanation.
- The `task-viewer is not running` absence, the `Stopping task-viewer (PID: ` and `Force stopping...` lines, the `status=0` line, and the empty kill-log assertion are unchanged, and the driver still prints the `foreign-alive` / `foreign-dead` tokens the Rust assertion reads.
- Staged check, run once and then reverted: put a fake `ps` first on `PATH` that prints `Z` for `-o stat= -p <pid>` and nothing else, run `cargo test -p task-viewer launcher_stop_guards_each_kill_against_a_foreign_pid`, and confirm it fails on the `foreign-alive` assertion; remove the fake and confirm the test passes again.
- `make test` passes.
