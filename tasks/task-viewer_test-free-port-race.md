---
description: "Stop reserving a process-test port by binding and releasing it, and retry start on the next candidate when the port is taken."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T01:48:47Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Remove the free-port race from the launcher tests

## Goal

The process-level launcher tests never fail because another process took the port between the test's probe and the launcher's bind. A taken candidate is skipped and the next one is tried.

## Context

`free_loopback_port` in `task-viewer/tests/launcher_process_safety.rs` binds `127.0.0.1:0`, reads the port, and drops the listener. Between that drop and the server's bind, any process on the machine can take the port, and two failure paths follow. When the launcher's own probe sees the port held, `start_binary` refuses with `Port $PORT is held by another process (PID: $holder); not starting task-viewer.` or the PID-less `Port $PORT is held by another process; not starting task-viewer.` When the port is taken after that probe, the detached server exits with `could not listen on` and `in use` in its log, and the launcher prints `Failed to start task-viewer. Check $LOG_FILE`. A test that starts one server then panics on its start-success assertion; a test that spawns two concurrent `start` invocations panics inside `wait_for_server` or `assert_one_start_won` instead.

`task-viewer/tests/graceful_shutdown.rs` already avoids this race: `candidate_ports` yields 16 ports without binding any, `Server::start_on` moves to the next candidate when the child's log reports `could not listen on` plus `in use`, and `listening_port` reads the port the child actually bound.

Each launcher copy carries one `PORT=7880` line, which `Root::from_dir` rewrites with the test's port, and the launcher exports that port to the server as `TASK_VIEWER_PORT`. A candidate list therefore re-stages the launcher copy for each candidate.

The launcher tests that never start a server keep `free_loopback_port`: the reused-PID, symlinked-alias, and lock-renamed-away cases probe a port they never bind. The port-conflict task deliberately holds a port so `start` refuses.

Live siblings share `TESTING.md`'s `- Launcher process safety:` bullet: [the port-conflict task](task-viewer_launcher-port-conflict-test.md) and [the guardrail claims task](archive/task-viewer_guardrail-status-and-radar-claims.md) rewrite it, [the moved-checkout task](task-viewer_launcher-moved-checkout-ownership.md) rewrites the port-holder branch this retry keys on, and the foreign-PID stop task verifies one of its sentences and adds none. This task's bullet edit merges into one pass with theirs.

## Approach

1. Replace the single reserved port with a bounded candidate list modeled on `candidate_ports` in `task-viewer/tests/graceful_shutdown.rs`: a deterministic range stepped per attempt, with no listener bound to reserve one. Re-stage the launcher copy for each candidate, because `Root::from_dir` rewrites the copy's single `PORT=7880` line and the launcher hands that port to the server as `TASK_VIEWER_PORT`.

2. Give the helper the `Root` constructor and the per-start command customization as parameters, or a per-attempt closure that receives the staged `Root`. The helper returns the winning `Root` and port, because `wait_for_server` and `stop_and_assert` read them, and the panic-after-start test passes a caller-owned root dir so its `Drop` assertion still sees it. `Root::new_under_a_space` in `a_checkout_path_with_a_space_recognizes_its_server` and the `.env("TASK_VIEWER_PORT", inherited.to_string())` on the start command in `inherited_task_viewer_port_does_not_move_the_bind` must survive the helper.

3. Retry the whole per-test attempt, not one spawn: construct a fresh `Root` per candidate — a new temp dir, or a caller-supplied dir cleaned between attempts, because `Root::from_dir` writes its binary symlink unconditionally and fails with `EEXIST` on a re-stage — including a stale lock the test staged, and re-spawn every start the attempt launches, so the tests that race two `start` invocations still race on one candidate and the stale-lock tests still reach the reclaim path.

4. Treat two outcomes as retryable: the launcher's port-held refusal, in both its PID and PID-less forms, and the server's own bind failure, read from the child's log as `could not listen on` plus `in use` the way `wait_until_serving` in `task-viewer/tests/graceful_shutdown.rs` does. A build failure, a missing binary, or any other start failure fails the test on the first attempt.

5. Cap the candidate list and fail with the last launcher output when every candidate is taken.

6. Apply the helper to every process-level launcher test that starts a server, including the tests that spawn two concurrent starts, the stale-lock tests, the space-path test, the inherited-port test, and the panic-after-start test. The launcher tests that never start a server keep `free_loopback_port`, and the port-conflict test keeps its deliberately held listener.

7. Add the helper's own tests: occupy the first candidate with a listener and assert the helper starts on the next; stage an attempt whose log carries `could not listen on` plus `in use` and assert the helper retries it on the next candidate; stage every candidate taken and assert the bounded failure names the last launcher output; and stage a fake `cargo` that exits non-zero and assert exactly one start attempt with no second launcher.

8. Record the retry rule in `TESTING.md`'s `- Launcher process safety:` bullet, in the same pass as the sibling bullet edits.

**Out of scope:** the launcher's own port handling, and the graceful-shutdown test.

## Acceptance

- A candidate the launcher reports as held causes a retry on the next candidate.
- An attempt whose server log carries `could not listen on` plus `in use` causes a retry on the next candidate, independently of the launcher's refusal message.
- A staged failing build makes the test fail on the first attempt, with exactly one start attempt and no second launcher.
- Every process-level launcher test that starts a server drives it through the helper; the tests that spawn two concurrent starts re-stage both starts on one candidate so they still race, and the stale-lock tests re-stage their lock on each attempt.
- With every candidate taken, the helper stops at the bounded list and its failure names the last launcher output.
- `TESTING.md`'s `- Launcher process safety:` bullet records the retry rule.
- The helper's tests and `make test` pass.
