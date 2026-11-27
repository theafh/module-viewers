---
description: "Test start's port-conflict refusal with a real foreign listener and the no-port-tool fallback, so a held port cannot be reported as a start."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Test the port-conflict refusal

## Goal

A launcher test proves that `start` refuses a port a foreign process holds, names the holder's PID inside its `(PID: N)` refusal, exits non-zero, and leaves the listener alive, and it fails when either refusal is removed. A second case proves the `port_answers` refusal when no port tool names the holder, and the `is_listening` fallback on a free port: a port that answers refuses the start without naming a PID, and a free port still starts through the fallback.

## Context

`start_binary` refuses a held port twice: when `port_pid` names a holder, and again when `port_answers` succeeds on a port the port tools cannot name:

```bash
local holder
holder=$(port_pid)
if [ -n "$holder" ]; then
    echo "Port $PORT is held by another process (PID: $holder); not starting task-viewer."
    return 1
fi
if port_answers; then
    echo "Port $PORT is held by another process; not starting task-viewer."
    return 1
fi
```

The excerpt is abridged: the `notify` calls between the branches are omitted.

`port_pid` runs before `port_answers`, so a machine with `lsof` or `ss` reaches the PID branch first, and only a port those tools cannot name reaches the message-only branch. `has_port_tool` is true when either tool exists, and `is_listening` returns `port_answers` when it is false:

```bash
is_listening() {
    is_running || return 1
    if has_port_tool; then
        [ "$(port_pid)" = "$(get_pid)" ]
    else
        port_answers
    fi
}
```

No test in `scripts/tests/`, `task-viewer/src/launcher_tests.rs`, or `task-viewer/tests/` contains either refusal message or drives a foreign listener against it. `port_answers` appears only as a stub returning false (the source-guard driver's `port_answers() { return 1; }`), and `has_port_tool` is never stubbed.

Two harnesses reach `start`. `task-viewer/src/launcher_tests.rs` copies the launcher verbatim through `stage_launcher`, so a test staged there runs on the real `PORT=7880` and a live dashboard can be the holder. `task-viewer/tests/launcher_process_safety.rs` rewrites the `PORT=7880` line to a free port and stages a fake `cargo`, `open`, and `osascript` on `PATH`.

Live sibling tasks share these surfaces. [The free-port race task](task-viewer_test-free-port-race.md) adds a candidate-list port helper for the same file and appends the retry rule to the same `TESTING.md` launcher bullet. [The moved-checkout ownership task](archive/task-viewer_launcher-moved-checkout-ownership.md) rewrites the same port-holder branch so a `task-viewer` binary from another path stops counting as "another process". The foreign-PID stop test verifies an existing sentence in that same `TESTING.md` bullet and adds none.

## Approach

1. Add the real-listener case to `task-viewer/tests/launcher_process_safety.rs`, the harness that rewrites `PORT=7880` to a free port. Hold the port with a non-task-viewer process, a test-owned `TcpListener` (or `python3 -m http.server`), so the moved-checkout ownership class stays distinct and this case keeps the "another process" refusal. Run the launcher's `start` and assert a non-zero exit, the holder's PID inside `(PID: N)`, and that the listener still accepts a connection afterwards.
2. Add a sourced-driver case in `task-viewer/src/launcher_tests.rs`, following the `START_TIMEOUT_DRIVER` precedent: stub `has_port_tool` to return false and `port_pid` to print nothing, so the real `port_pid` cannot take the PID branch. Refusal side: keep `is_running` false and stub `port_answers` to return true, then assert the PID-less refusal with a non-zero exit. Success side: run this half in `task-viewer/tests/launcher_process_safety.rs`, whose harness rewrites the port, with `has_port_tool` reporting no tool and `port_answers` returning false on a free port, so the real `is_listening` fallback runs and the start succeeds. A sourced driver in `launcher_tests.rs` cannot retarget the exported `TASK_VIEWER_PORT`, and stubbing `is_listening` would leave the fallback untested.
3. Keep the existing successful-start cases.
4. Rewrite the `TESTING.md` bullet that opens `- Launcher process safety:` to name the port-conflict and no-port-tool coverage, and add the held-port refusal to the `FEATURES.md` launcher bullet that carries `A start that never gets its server onto the port terminates the process it launched.`
5. Control the port inside both new cases: the real-listener case binds the listener it keeps, and the sourced driver needs no port, so neither case depends on the candidate-list helper the free-port race task owns.

**Out of scope:** changing the refusal message, the `lsof`/`ss` selection, and the candidate-list helper, which the free-port race task owns.

## Acceptance

- The real-listener case fails when the `[ -n "$holder" ]` refusal is removed, and it asserts the holder's PID inside `(PID: N)` plus a listener that still answers after `start` exits.
- The no-tool refusal case fails when the `port_answers` refusal is removed, and it asserts the PID-less message and a non-zero exit.
- The no-tool free-port case starts successfully through the real `is_listening` fallback, and it fails if that fallback refuses a port that does not answer.
- `TESTING.md`'s `- Launcher process safety:` bullet names the port-conflict and no-port-tool coverage, and `FEATURES.md`'s launcher bullet states the held-port refusal.
- The new cases and `make test` pass.
