---
description: "Stop the launcher from wedging on a stale lock whose PID was reused, and make a restart that cannot stop the old server fail instead of reporting success."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-25T00:15:31Z
status: implemented
implemented-by: Andreas Hoffmann
design-extended: false
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
3. Settle what "names the launcher" means, and use one definition for both the recorded line and the live line: a command line names the launcher when any whitespace-separated token, resolved, equals the resolved own script path — compare every token rather than only the first, because a real holder's line can carry the launcher behind an interpreter (`bash --noprofile --norc <launcher> status`). Resolve a token with mechanisms the launcher's declared floor already carries: resolve its directory with `cd "$(dirname "$token")" && pwd -P` and follow the final component through `readlink` while it stays a symlink, joining a relative target to the directory that holds it; resolve the waiter's own `${BASH_SOURCE[0]}` the same way, through `command -v` first when the invoked name carries no `/`. Normalise both resolved paths through `cd "$(dirname "$path")" && pwd -P` immediately before comparing them, so a relative symlink target carrying `..` collapses to its canonical directory instead of surviving into the comparison, and fall back to a token's basename only when its walk fails, so the fallback recognises a launcher invoked under its own name from any directory and not an alias whose basename differs — the alias case is decided by the walk, which uses `readlink` rather than `realpath`, which the installed app's `LSMinimumSystemVersion` 11.0 floor (`scripts/install-desktop.sh`) may lack. This recognises the same launcher reached through a symlinked alias instead of reclaiming a live holder.
4. In `start_binary`, make the restart branch propagate a failed stop: `do_stop || return 1`. The `toggle` and `restart` callers already return `start_binary`'s status.
5. Update the header comments that describe the recorded-line-only rule, and extend `TESTING.md`'s launcher bullet with the recorded-and-live rule, the resolved-alias case, and the restart-failure behaviour the next item adds.
6. Add cases in `task-viewer/tests/launcher_process_safety.rs` for the reused-PID wedge and the alias, and sourced-driver cases in `task-viewer/src/launcher_tests.rs` for a restart whose stop fails and for a holder whose live `ps` read comes back empty (stubbing `ps` over the lock wait, as `IS_OURS_DRIVER` already stubs it for `is_ours`). Bound each wait with a timeout so a regression fails the test instead of hanging the suite.

**Out of scope:**

- The 5-second stop window and the 4-second shutdown grace; both are documented and unchanged.
- The `PORT` constant and the `TASK_VIEWER_PORT` hand-off; `tasks/archive/task-viewer_launcher-process-safety.md` settles that design.

## Acceptance

- [ ] A process-level case writes a lock directory whose `pid` names a live unrelated process and whose `cmd` names the launcher, runs `status` under a bounded wait, and asserts it exits with the stopped status instead of timing out; the case hangs on the current script.
- [ ] A launcher case stubs the stop helper to return 1 on the outdated-binary branch and asserts `start` exits non-zero and prints no "already running" line.
- [ ] A sourced-driver case writes a lock whose `pid` names a live process and whose `cmd` names the launcher, stubs `ps` to print nothing for that PID, and runs `acquire_lock` in the background: after a fixed window the lock still records the holder's PID, and once the lock is released the call acquires it and proceeds; the case fails against an implementation that reads an empty live read as "not a holder", and `launcher_status_waits_for_the_lock_holder` stays green.
- [ ] While a canonical-path invocation holds the lock, an invocation through a symlinked alias named `tv` — a basename the holder's recorded command line does not contain, created with a relative target that carries `..` (for example `ln -s ../scripts/task-viewer.sh bin/tv`) — is still running after a fixed window and finishes once the lock is released, so the alias waits on the live holder instead of reclaiming it; the current script lets that `tv` alias reclaim the lock and proceed without waiting.
- [ ] `make lint` and `make test` pass, and every existing launcher case stays green.
