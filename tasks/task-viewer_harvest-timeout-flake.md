---
description: "Make the intermittent harvest git-timeout non-retry test deterministic, and drop the gate carve-outs that name it."
scope: task-viewer/src
created: 2026-11-13T20:47:07Z
updated: 2026-11-14T23:52:55Z
status: ready
reported-by: Andreas Hoffmann
---

# Make the git-timeout non-retry test deterministic

## Goal

`harvest_timeout_does_not_retry_the_git_dir_fallback` passes on every run, on an idle machine and under load, while proving three things: a git command that outlives its deadline fails the scan instead of taking the `updated` fallback, the timed-out command runs exactly once, and the failure is the deadline's rather than an ordinary git error. The standing `make test` gate then holds without an exemption, so every live task that carves this test out of its gate drops the carve-out.

## Context

- Build order: ships before `task-viewer_chart-a11y-and-cost.md`, `task-viewer_chart-geometry-and-labels.md`, and `task-viewer_ui-labels-and-contrast.md`, each of which carries a gate carve-out this task removes.
- `task-viewer/src/harvest.rs` holds the test in its `#[cfg(test)] mod tests`, as `fn harvest_timeout_does_not_retry_the_git_dir_fallback`. It builds a scratch repository with one live task and one archived task whose status is `open`, so that task's leave day comes from `fn git_pickaxe`, installs a stand-in `git`, and scans once per attempt.
- The stand-in appends its arguments to a `calls` file inside its `*--follow*` arm and then `exec sleep 10`; the test counts the `--follow` lines in that file. `fn register_fake_git` registers a 100 ms deadline for the fixture repository, and `fn register_fake_git_with_deadline` starts the stand-in once at a 1-second deadline before registering it.
- The flake: the deadline starts when the child spawns, so on a loaded machine the 100 ms can expire before `/bin/sh` reaches its `echo`. The `calls` file then stays empty, and the six-attempt retry loop can lose that same race on every attempt, leaving a zero count against `assert_eq!(follow_calls, 1, …)`.
- The sibling timeout tests register the same 100 ms deadline and assert only that the scan fails, which a stand-in killed before its first line still satisfies, so the race reaches only this counting test.
- `fn git_timeout_failure` builds `GitFailure::Timeout` with the message `git did not finish within {} ms`, and `fn git_pickaxe` returns that reason through `LogLookup::Failed`; `HarvestError::Git` carries only that string, so the message is the one surface that separates a deadline expiry from an ordinary git failure.
- `TESTING.md` requires deterministic, isolated, offline, and parallel-safe tests.
- The live tasks whose gate item names this test, found at implementation time by `grep -l harvest_timeout_does_not_retry_the_git_dir_fallback tasks/task-viewer_*.md`, each carry the gate exemption this task retires. Today the selector matches [task-viewer_chart-a11y-and-cost.md](task-viewer_chart-a11y-and-cost.md) and [task-viewer_chart-geometry-and-labels.md](task-viewer_chart-geometry-and-labels.md), which link here as the flake's owner, and `tasks/task-viewer_ui-labels-and-contrast.md`, which names the test without linking here, so this task ships before each of them is implemented.
- Prior art: [task-viewer_git-command-timeout.md](archive/task-viewer_git-command-timeout.md) shipped the bounded-command helper this test measures.

## Approach

- Register this fixture's deadline with `register_fake_git_with_deadline(&repo, &script, Duration::from_secs(2))` and change the hanging arm to `exec sleep 30`, so the deadline clears worst-case process start by two orders of magnitude while the hang still exceeds the deadline fifteenfold. Keep `fn register_fake_git`'s 100 ms default for the sibling timeout tests.
- Replace the six-attempt retry loop and its `fs::remove_file(&calls)` reset with one `harvest_repository(&repo)` call and one read of the `calls` file, so a single scan proves the property instead of accepting up to six.
- Keep both assertions and add the timeout message: assert `HarvestError::Git` whose text contains `git did not finish within 2000 ms`, and assert the `--follow` count is one. The message cannot pass alone, because a stand-in that never starts times out on an earlier command with an empty `calls` file; the count cannot pass alone, because an ordinary git failure also ends the scan after one invocation.
- Delete the exemption clause from the gate item of every live task that names this test, found at implementation time by `grep -l harvest_timeout_does_not_retry_the_git_dir_fallback tasks/task-viewer_*.md` with this file excluded, leaving each item's own gate sentence in place.

**Out of scope:** The production deadline, `fn git_timeout`'s parsing, and the timeout semantics themselves stay as they are; this task changes only the fixture that measures them.

## Acceptance

- The test asserts the failure is `HarvestError::Git` whose message contains `git did not finish within 2000 ms`, and asserts the recorded `calls` file holds exactly one `--follow` line; one run records both.
- The test body carries a single `harvest_repository(&repo)` call and a single read of the `calls` file, with the retry loop gone.
- Ten consecutive runs of `cargo test -p task-viewer harvest_timeout_does_not_retry_the_git_dir_fallback`, taken in one shell loop on the same tree, each print `test result: ok`. Fail branch: a run that fails means the deadline margin is too small on that machine, so raise it, re-pin the `Duration::from_secs(…)` value in this task's first Approach bullet and in the fourth Acceptance item, re-pin the `2000 ms` message text in the third Approach bullet and in the first Acceptance item, and repeat the ten; restoring the retry loop or dropping the exactly-one assertion is not a repair.
- The test registers its fixture with `Duration::from_secs(2)` and hangs with `sleep 30`, `fn register_fake_git` still registers 100 ms, and `cargo test -p task-viewer harvest_` passes on the same tree.
- A grep for `harvest_timeout_does_not_retry_the_git_dir_fallback` over `tasks/task-viewer_*.md` names only this task, and `git diff -- tasks/` shows each gate item that exempted the test now carrying its own gate sentence alone.
