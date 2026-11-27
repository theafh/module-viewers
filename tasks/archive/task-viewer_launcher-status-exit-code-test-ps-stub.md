---
description: "Stub `ps` in the launcher status exit-code test so PID 4242's real process line cannot add a Started-from line and break the byte-exact assertion."
scope: task-viewer
created: 2026-11-27T21:05:42Z
updated: 2026-11-27T22:44:27Z
status: finished
implemented-by: "Andreas Hoffmann"
design-extended: false
reported-by: "Andreas Hoffmann"
---

# Stub `ps` in the launcher status exit-code test

## Goal

The launcher test `launcher_status_exit_code_reports_whether_the_server_runs` in `task-viewer/src/launcher_tests.rs` isolates `do_status` completely: it stubs `ps` so a `-ww` read prints nothing, and its driver comment states that both `is_running` and `ps` are stubbed. The test's byte-exact stdout assertion then depends only on `do_status`'s own contract, on every host and whatever process happens to hold PID 4242.

## Context

`do_status` in `scripts/task-viewer.sh` prints its two running lines, then runs `started=$(started_from_path "$(get_pid)" 2>/dev/null)` and prints `Started from: $started` when that path differs from `$BINARY`. `started_from_path` reads the real process table through `ps -ww -p "$candidate" -o args=`.

The test's driver stubs `get_pid() { echo 4242; }` and `is_running() { return 0; }` but leaves `ps` real. On a host where PID 4242's `ps` line names a task-viewer release binary from another path, `started_from_path` returns that path and `do_status` emits a third `Started from:` line, so the exact-stdout assertion fails for a reason unrelated to `do_status`. The comment above the driver claims `is_running` is overridden "so the test measures `do_status`'s own contract and nothing about process lookup", which is false while the real `ps` answers.

Two sibling drivers in the same file already stub `ps` for a `status` run: `MOVED_STATUS_DRIVER` and `SAME_CHECKOUT_STATUS_DRIVER` print a staged line for a `-ww` read, and `EMPTY_LIVE_PS_DRIVER` uses `ps() { return 0; }` to model a read that prints nothing.

`TESTING.md`'s `- Launcher process safety:` bullet already states the status contract this test backs, including the started-from line, so the fix leaves that sentence accurate and unchanged.

## Approach

1. In the `launcher_status_exit_code_reports_whether_the_server_runs` driver, add a `ps` stub in the `EMPTY_LIVE_PS_DRIVER` shape, `ps() { return 0; }`, so a `-ww` read prints nothing and `started_from_path` returns failure for the faked PID. Place it beside the existing `get_pid` and `is_running` stubs, before the two `do_status` calls.
2. Rewrite the comment above the driver in place so it states that both `is_running` and `ps` are stubbed, and that the test measures `do_status`'s own output and exit codes. Drop the "nothing about process lookup" clause that the real `ps` call made untrue.
3. Leave the exact-stdout assertion as it is: the two running lines, the `---stopped---` marker, and the one stopped line.

**Out of scope:** adding `ps` stubs to the other launcher drivers. Each driver is audited by this task's acceptance selector, and a driver that already stubs `ps` keeps its current stub.

## Acceptance

- In `task-viewer/src/launcher_tests.rs`, the `launcher_status_exit_code_reports_whether_the_server_runs` driver defines a `ps` function whose `-ww` read prints nothing, so `started_from_path` cannot report a started-from path for the faked PID.
- The comment above that driver names both the `is_running` stub and the `ps` stub, and no longer says the test measures nothing about process lookup.
- `cargo test -p task-viewer launcher_status_exit_code_reports_whether_the_server_runs` passes, and its exact-stdout assertion still carries the two running lines, `---stopped---`, and the stopped line.
- Sensitivity check, run once and then reverted: temporarily make the driver's `ps` stub print a foreign release path such as `/tmp/elsewhere/target/release/task-viewer` for a `-ww` read, confirm the exact-stdout assertion fails on the extra `Started from:` line, then restore the printing-nothing stub and confirm the test passes again.
- Every launcher driver in `task-viewer/src/launcher_tests.rs` that stubs `is_running` and calls `do_status` also stubs `ps`; find them by searching the file for `do_status`, and report any driver that does not.
