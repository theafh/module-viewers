---
description: "Stop the launcher from wedging on a stale lock whose PID was reused, and make a restart that cannot stop the old server fail instead of reporting success."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-24T22:41:46Z
status: ready
reported-by: Andreas Hoffmann
---

# Launcher lock and restart truth

## Goal

Two launcher failure paths stop lying. A stale lock left by a killed holder no longer hangs every later `start`, `stop`, `status`, and Dock click once the operating system reuses that PID, and a restart that cannot stop the old server exits non-zero instead of reporting "already running" as a success.

## Context

`scripts/task-viewer.sh` holds the lock protocol and the start path. Both defects sit in that one script. The lock is staged by the process-level suite `task-viewer/tests/launcher_process_safety.rs`, and the sourced-driver cases live in the inline module `task-viewer/src/launcher_tests.rs`.

The lock branch reads:

```sh
if [ -n "$holder" ] && holder_is_launcher "$holder_cmd" &&
    kill -0 "$holder" 2>/dev/null; then
    sleep 0.1
    continue
fi
```

`holder_cmd` is read from `$LOCK_DIR/cmd`, which the *dead* holder wrote. The comment above the branch says a reused PID "would wedge the launcher forever", and `TESTING.md` promises that "a lock whose recorded PID belongs to a live process whose recorded command line does not name this launcher is reclaimed". The code satisfies that sentence, but it leaves the dangerous shape uncovered: the recorded line *does* name the launcher, so a reused PID still matches. When the launcher is SIGKILLed while holding the lock, `pid` and `cmd` survive; the OS later reuses the PID for an unrelated long-lived process; every later command then loops on `sleep 0.1` with no bound and no output. The existing `a_reused_pid_lock_is_reclaimed` test writes `cmd` as `sleep 120`, so it covers only the non-launcher-recorded shape.

The start path calls the stop helper and ignores its result:

```sh
if is_running && is_outdated; then
    echo "A newer task-viewer binary exists; restarting on it."
    do_stop
fi
if is_running; then
    echo "task-viewer is already running (PID: $(get_pid))"
    ...
    return 0
fi
```

`do_stop` returns 1 when the process survives even `kill -9`. That status is discarded, `is_running` is then still true, and the function prints the success message and returns 0, contradicting the stop contract in `TESTING.md` ("a stop whose process survives `kill -9` exits non-zero") and the restart promise in `FEATURES.md` section 1.

Two constraints shape the lock fix. The script already treats a failed `ps` read as possible and falls back to `$0` when recording, so a live read that fails must never be read as "not a holder": reclaiming a live lock would run two commands at once and break `FEATURES.md`'s "One launcher command runs at a time". The branch also follows a missing-record grace block that gives a holder a moment to write `pid` and `cmd`; the existing `launcher_status_waits_for_the_lock_holder` case depends on it.

## Approach

1. In `acquire_lock`, read the live command line with `ps -ww -p "$holder" -o args=`. Wait when the recorded line names the launcher, the PID is alive, and either the live line also names the launcher or the live read failed (empty). Fall through to the reclaim path only when the live read succeeded and does not name the launcher.
2. Leave the missing-record grace block unchanged, and keep its retry count.
3. Resolve the launcher path inside a command line before comparing: resolve both the holder's token and the waiter's own script path through their symlinks (for example `realpath`), and fall back to the basename only when a token cannot be resolved. This recognises the same launcher reached through a symlinked alias instead of reclaiming a live holder.
4. In `start_binary`, make the restart branch propagate a failed stop: `do_stop || return 1`. The `toggle` and `restart` callers already return `start_binary`'s status.
5. Update the header comments that describe the recorded-line-only rule, and extend `TESTING.md`'s launcher bullet with the recorded-and-live rule, the resolved-alias case, and the restart-failure behaviour the next item adds.
6. Add cases in `task-viewer/tests/launcher_process_safety.rs` for the reused-PID wedge and the alias, and a sourced-driver case in `task-viewer/src/launcher_tests.rs` for a restart whose stop fails. Bound each wait with a timeout so a regression fails the test instead of hanging the suite.

**Out of scope:**

- The 5-second stop window and the 4-second shutdown grace; both are documented and unchanged.
- The `PORT` constant and the `TASK_VIEWER_PORT` hand-off; `tasks/archive/task-viewer_launcher-process-safety.md` settles that design.

## Acceptance

- [ ] A process-level case writes a lock directory whose `pid` names a live unrelated process and whose `cmd` names the launcher, runs `status` under a bounded wait, and asserts it exits with the stopped status instead of timing out; the case hangs on the current script.
- [ ] A launcher case stubs the stop helper to return 1 on the outdated-binary branch and asserts `start` exits non-zero and prints no "already running" line.
- [ ] A live holder whose `ps` read fails is still waited for, and the missing-record grace case stays green.
- [ ] A symlinked alias of the script running concurrently with the canonical path serialises on the lock instead of reclaiming it.
- [ ] `make lint` and `make test` pass, and every existing launcher case stays green.
