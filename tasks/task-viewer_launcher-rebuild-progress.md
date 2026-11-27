---
description: "Announce every launcher rebuild and print one wait line when a second command waits on the lock, instead of a silent multi-minute click."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T17:27:02Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Show rebuild progress and lock waits

## Goal

Every launcher build prints and notifies that it is building, not only the first run. A command that waits for another launcher command prints one line naming the holder's PID and sends one notification on that first wait, then stays quiet until it proceeds.

## Context

`build_binary` in `scripts/task-viewer.sh` announces only when the binary is missing:

```bash
if [ ! -x "$BINARY" ]; then
    echo "Building task-viewer..."
    notify "task-viewer" "Building release binary (first run)…"
fi
if ! cargo build --release --locked -p task-viewer >"$BUILD_LOG" 2>&1; then
```

Every later rebuild after a source change sends cargo's output to `$BUILD_LOG` and shows nothing. The whole command, build included, runs under `acquire_lock`, whose live-holder branch sleeps and retries with no message, so a second Dock click or `make status` appears to hang for the build's duration.

An icon click has no terminal attached: the macOS bundle execs the launcher directly and the Linux desktop entry sets `Terminal=false`, so a printed line alone reaches nobody on that path. The first wait therefore also notifies, through the same helper the build announcement uses.

Three documents carry the launcher's behaviour and are edited by sibling tasks as well. The launcher paragraph in `FEATURES.md` and the launcher state paragraph in `README.md` are both written by [task-viewer_launcher-relative-state-dir.md](task-viewer_launcher-relative-state-dir.md), the `FEATURES.md` paragraph plus a planned status line-set assertion in `task-viewer/src/launcher_tests.rs` are written by [task-viewer_guardrail-status-and-radar-claims.md](archive/task-viewer_guardrail-status-and-radar-claims.md), and the `FEATURES.md` launcher bullet, the `README.md` launcher sentence, and the `TESTING.md` launcher process-safety bullet are also written by [task-viewer_launcher-moved-checkout-ownership.md](archive/task-viewer_launcher-moved-checkout-ownership.md); whichever lands later rebases its sentence onto the others. The launcher process-safety bullet in `TESTING.md` is the coverage ledger the launcher test suites extend.

## Approach

1. Print and notify `Building task-viewer…` before every `cargo build`, not only when the binary is missing. Keep the build log redirect.
2. In `acquire_lock`, print one line and send one notification the first time it waits on a live holder, naming the holder's PID and that another task-viewer command is running. Stay silent on every later retry, and keep the once-flag at file scope beside `LOCK_HELD` rather than in a `local`: `require_lock_ownership` re-enters `acquire_lock` after a lost lock, and a local flag would let the line print a second time.
3. Keep the failed-build message and the existing bounded lock failure unchanged.
4. Add the tests in the suite each case belongs to. Put the live-holder wait-line case in `task-viewer/src/launcher_tests.rs` beside `launcher_lock_waits_when_the_live_read_is_empty`, modelled on `EMPTY_LIVE_PS_DRIVER`, which already stubs `notify`; add a second wait episode by re-entering `acquire_lock` through `require_lock_ownership`, because that driver produces only one, and assert one line naming the holder's PID, one notification, and no repeat. Put the slow-build second-run case in `scripts/tests/launcher-build-truth.sh`, whose `make_root` stages a prebuilt binary and whose case adds its own cargo stub. The shared notify stub logs only the bare tool name, and `case_missing_cargo_fails` matches that whole line, so either overwrite the `osascript` and `notify-send` stubs inside the new case to log `"${0##*/} $*"`, or add `case_missing_cargo_fails` to this task's edit set and change its `grep -qx "$tool"` to `grep -q "^$tool "`; then assert the building message and the build notification. Add a failing-cargo case to the same suite that asserts `Build failed. Check $BUILD_LOG` and that no server starts.
5. Record the messages by rewriting the launcher paragraph in `FEATURES.md` and the launcher state paragraph in `README.md` in place, and extending the `TESTING.md` launcher process-safety bullet with the new cases. State that a contended `status` prints the wait line before its own output, so the status line-set wording and assertion planned by `task-viewer_guardrail-status-and-radar-claims.md` account for it.

**Decision:** The first wait notifies as well as prints, mirroring the build announcement, so a contended icon click is not silent. The notification fires once per wait episode, on the same first-wait flag as the line.

**Out of scope:** streaming cargo's full output to the terminal, and changing the lock protocol.

## Acceptance

- A rebuild after a source change prints and notifies that it is building, asserted by the slow-build case in `scripts/tests/launcher-build-truth.sh` through the `NOTIFY_CALL_LOG` stubs.
- A second launcher command waiting on a live holder prints one line naming the holder's PID and sends one notification, and a second wait episode prints and notifies no second time; the case sits in `task-viewer/src/launcher_tests.rs`.
- A failing cargo build with the binary present reports `Build failed. Check ` followed by the resolved build-log path and starts no server, asserted by the failing-cargo case in `scripts/tests/launcher-build-truth.sh`.
- `FEATURES.md`'s launcher paragraph, `README.md`'s launcher state paragraph, and `TESTING.md`'s launcher process-safety bullet each state the build announcement and the single wait line, the contended `status` line is described ahead of the status output, and a grep for the first-run-only build wording in `scripts/task-viewer.sh` returns nothing.
- `make test` passes.
