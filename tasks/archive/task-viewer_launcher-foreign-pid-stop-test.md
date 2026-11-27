---
description: "Drive do_stop past its early return with a foreign PID so the is_ours guard before each kill is actually exercised."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T21:16:25Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Test the stop guard against a foreign PID

## Goal

A launcher test fails when either `is_ours` guard before a kill is removed. The test proves that a stop which reaches the kill path with a foreign PID sends it no signal.

## Context

`do_stop` returns early when `is_running` is false:

```bash
if ! is_running; then
    echo "task-viewer is not running"
    return 0
fi
```

and guards both kills with `if is_ours "$pid"; then`. The existing `launcher_stop_leaves_a_foreign_process_alone` test stubs `port_pid` to return nothing, so `is_running` is false and `do_stop` returns at the early return; the `is_ours` guards before `do_stop`'s kills are never reached. A lens deleted both guards from a copy of the launcher and the test still passed with the same output.

The test targets the post-change `is_ours` and `do_stop`: [the moved-checkout ownership task](task-viewer_launcher-moved-checkout-ownership.md) changes both and adds launcher tests in the same file, so this work follows it.

## Approach

1. Add a launcher test driver that makes `is_running` return true and `get_pid` return a PID that belongs to a live foreign process, while keeping the real `is_ours` and the real kill sites. Launch that process with `command sleep 60 &` before any `sleep` override, and add the `sleep() { :; }` stub that skips `do_stop`'s wait loop only after it, so the stub cannot turn the foreign job into a no-op subshell.
2. Run `do_stop` and assert the foreign process is still alive and the kill log is empty. Run the liveness probe after `unset -f kill`, so the logging `kill` stub cannot answer `kill -0` for a dead process.
3. Verify the test fails when either `is_ours` guard before a kill in `do_stop` is removed, and record that check in a comment on the test.
4. Keep the existing early-return test; the two cases cover different paths.
5. Verify that `TESTING.md`'s launcher bullet keeps the existing sentence `a stop aimed at an unrelated PID sends it no signal`; the new test covers that documented case, so add no new sentence.

**Out of scope:** changing the stop protocol, and the survivor-after-`kill -9` case, which already has a test.

## Acceptance

- The new test drives `do_stop` past its early return with a foreign PID.
- The foreign process receives no signal and stays alive.
- Removing either `is_ours` guard before a kill in `do_stop` makes the test fail.
- The existing launcher tests and `make test` pass.
