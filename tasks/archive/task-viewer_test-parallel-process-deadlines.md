---
description: "Make the two fixed five-second test deadlines immune to process-start latency under the parallel suite, with the margin measured rather than guessed."
scope: task-viewer/src
created: 2026-11-17T17:55:59Z
updated: 2026-11-17T22:26:55Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: false
---

# Make the fixed test deadlines survive a loaded parallel suite

## Goal

`harvest::tests::run_git_command_drains_output_larger_than_a_pipe_buffer` and `web::tests::web_scan_keeps_earlier_rows_when_a_ref_union_blob_vanishes` pass on every run of the parallel `make test` suite, on an idle machine and on a loaded one, because each measures the work it targets rather than the latency of starting a process. Each site carries the measurement that justifies its deadline, and `TESTING.md` states the rule once so a later test does not reintroduce a guessed constant.

## Context

- `make test` runs `cargo test -p task-viewer` with no `--test-threads` argument (the `test:` target in `Makefile`), so cargo runs the suite in parallel, which `TESTING.md`'s "Keep tests deterministic, isolated, offline, and parallel-safe" bullet already states.
- Both tests pass a fixed five-second deadline. `harvest.rs`'s `run_git_command_drains_output_larger_than_a_pipe_buffer` passes `Duration::from_secs(5)` to `run_git_command` and fails with `large output must not time out`; `web.rs`'s `web_scan_keeps_earlier_rows_when_a_ref_union_blob_vanishes` passes `Duration::from_secs(5)` to `harvest::register_test_git` and fails with `git did not finish within 5000 ms`.
- The observation: a review gate ran `make test` twice on 2026-11-17 on a machine at load average about 12 on 14 cores, and both runs reported `261 passed; 2 failed`, the two tests above. Both passed in isolation, and the suite passed with `--test-threads=1`.
- The failed reproduction attempt: five consecutive full-suite runs with both deadlines at five seconds, under twelve synthetic CPU burners with load average rising from 5.66 to 28.48, each reported a passing suite. A single 256 KiB `head | tr` pipeline measured 10-30 ms. The trigger is therefore not established, and a larger constant is not yet justified by any measurement.
- `harvest.rs` already holds two patterns for this class. `fn register_fake_git_with_deadline` starts the stand-in once at a 60-second warm-up deadline before registering a short one. `fn run_git_command_times_out_and_kills_the_child` doubles its deadline until the stand-in demonstrably records its pid, and its comment states that "no fixed value survives an arbitrarily loaded machine under a parallel test run".
- Prior art: [task-viewer_harvest-timeout-flake.md](task-viewer_harvest-timeout-flake.md) repaired this class for `harvest_timeout_does_not_retry_the_git_dir_fallback` and recorded the measurement that justified its margin: "A 2 s deadline failed 7 of 20 full-suite runs under parallel load, so the margin is 10 s", from a start-latency race where "the deadline starts when the child spawns".

## Approach

- Reproduce first. Build a load harness that runs the full suite repeatedly under a fixed CPU load, and measure process-start latency the way these tests experience it: spawn a stand-in script and time it from spawn to its first output. Record the worst case, the load, and the run count.
- Repair each site by warming the stand-in before the measured call, the one pattern both sites can carry. At the harvest site warm the script the way `fn register_fake_git_with_deadline` already does. At the web site warm it inside `harvest::register_test_git`, which takes the single `Duration` it registers, returns nothing, and `web.rs` calls it twice — at `web_scan_keeps_earlier_rows_when_a_ref_union_blob_vanishes` and at `web_scan_keeps_earlier_rows_when_the_enclosing_probe_fails`, both with `Duration::from_secs(5)` — so a warm-up inside that helper covers both callers, and the enclosing-probe test's existing warm-up before its own registration stays as it is. Retry is not the pattern at either site: one registration governs every git command in the scan, and `web.rs` cannot reach the private `run_git_command` to retry a call against a marker. Both sites follow the one rule that `TESTING.md`'s Test Design Principles states.
- Set each deadline to `max(5 s, measured worst case × 5)`. The stated margin is that factor of 5×, taken from the archived `task-viewer_harvest-timeout-flake` precedent recorded in `## Context`, which raised a 2 s deadline that failed 7 of 20 loaded full-suite runs to 10 s — the only recorded margin measurement in this repo. Five seconds is the floor, and the measured worst case times 5× governs when process start dominates: `harvest.rs`'s `run_git_command` calls `spawn()` before it sets `let started = Instant::now()`, so the budget begins only after `spawn()` returns and the child's own startup and scheduling wait are charged against it, which is exactly the case where five seconds fails. Record the protocol and the number in a comment at each constant, so a later reader sees why the value is what it is.
- State the rule once in `TESTING.md`'s Test Design Principles: a test that passes a wall-clock deadline for process work carries the measured start latency that justifies it, and warms or retries rather than guessing.
- The two tests keep proving what they prove today: the drain test asserts the 256 KiB stdout and stderr contents, and the vanished-blob test asserts the scan's recorded error names the path.

**Out of scope:** The production deadline `GIT_COMMAND_TIMEOUT` and its parsing stay as they are, so this task changes only test fixtures. A suite-wide parallelism change, such as a shared semaphore over git-spawning tests, stays out as well, because the suite already passes under load and no measurement yet shows the scheduler is the cause.

## Measurement

Recorded 2026-11-17 on a 14-core macOS 26.6.2 machine.

- Harness: 12 CPU burners (`yes > /dev/null`) held for the whole run, with a full `make test` in flight, while a probe spawned the drain test's stand-in script and timed it from `spawn()` returning to its first stdout byte. That is the span `run_git_command` charges against its deadline, because it sets `let started = Instant::now()` after `spawn()` returns and only then waits for stdout to close.
- Run count: 3 loaded full-suite runs, 300 samples each, 900 samples total.
- Load: 1-minute load average 14.95, 17.59, and 22.48 at the three measurement windows, up from an idle baseline of 4.17-6.48.
- Worst case: 72.69 ms, from the run whose window read load average 17.59. Idle baseline over 300 samples was 41.57 ms.
- Harness command: `measure-under-load.sh 300 12`, which holds the burners, starts `make test`, and runs `probe drain.sh 300` while the suite is in flight. The probe is a throwaway `rustc -O` binary and the script a copy of the drain test's stand-in, both kept outside the repo.
- Derivation: `max(5 s, 5 x 72.69 ms = 363 ms)` is 5 s, so the 5 s floor still governs and neither deadline value changes. The warm-up is what removes the first-start cost the deadline would otherwise charge.
- Ten-run protocol: 10 consecutive full `make test` runs under the same 12-burner harness each reported a passing suite, with no failure of either test.

## Acceptance

- Both tests carry a comment naming the measured worst-case process-start latency, the load it was measured under, and the run count, and their deadline or warm-up value is derived from that number.
- The measurement protocol is recorded in this task's body as its run count, its load, and its harness command, and the recorded worst case is written beside each constant.
- Ten consecutive full `make test` runs under the recorded load harness each report a passing suite, with no failure of either test.
- The drain test asserts 262144 `o` bytes on stdout and 262144 `e` bytes on stderr, and the vanished-blob test asserts the scan's recorded error names `tasks/scope_vanished.md`, so a warm-up restructure that drops either assertion fails this item.
- A grep over `task-viewer/src` for a `Duration::from_secs` argument passed to `run_git_command` or `register_test_git` at the two sites this task repairs — `harvest.rs`'s drain-test argument and the vanished-blob test's `register_test_git` argument — finds no value smaller than the measured worst case without a comment stating its measurement.
- `TESTING.md`'s Test Design Principles carries the deadline rule once, and no other passage states a second version of it.
- If the harness records no failure across its protocol, the task records that outcome and adopts the measured worst case times the stated margin, and restoring a guessed constant with no measurement is not a repair.
- When a run in the ten-run protocol fails, the task re-measures process-start latency at that run's load, enlarges the margin from the new worst case, writes the new value and measurement beside the deadline or warm-up argument at the failing site, and restarts the full ten-run protocol from run one.
