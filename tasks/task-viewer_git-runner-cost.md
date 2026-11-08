---
description: "Wait for git children on their output instead of a 5 ms poll, resolve the git binary and PATH once, and let TASK_VIEWER_GIT_TIMEOUT_SECS raise the 30 s git deadline."
scope: task-viewer
created: 2026-11-08T14:13:24
updated: 2026-11-08T15:46:20
status: ready
reported-by: "Andreas Hoffmann"
---

# Wake git runners on output, resolve git once, and make the deadline configurable

## Goal

Every read-only git subprocess that harvest starts is noticed the moment its output ends instead of on the next 5 ms poll, idles without waking while git works, and keeps the same hard deadline. The git binary and the `PATH` handed to git are resolved once per process instead of once per subprocess. A person whose repository needs longer than 30 seconds for one git command can raise the deadline with `TASK_VIEWER_GIT_TIMEOUT_SECS`, the way `TASK_VIEWER_SCAN_WORKERS` sets the worker cap. Rows, fallbacks, and scan errors stay as they are for every repository that finishes inside the deadline.

## Context

Both runners are in `task-viewer/src/harvest.rs`. The buffered runner drains stdout and stderr on their own threads, feeds stdin from a third, and loops on `child.try_wait()` with `std::thread::sleep(Duration::from_millis(5))` until the child exits or the deadline passes. The streaming runner `run_git_streaming` parses stdout on the calling thread while a watcher thread runs the same 5 ms `try_wait` loop and also checks an `abort` flag. Each finished child is therefore seen up to 5 ms late. A cold harvest of this repository measured 117 git processes in 2.7 seconds, against 6 processes in 125 ms warm, so the per-process delay sits mostly in the first scan.

`scrub_git_env` sets `PATH` from `git_path()` for every command, and `git_path()` rebuilds a `Vec<PathBuf>` from `GIT_SEARCH_DIRS` plus the inherited `PATH` and joins it each time. `git_program` returns `git_bin()` in production, and `git_bin()` stats up to three candidate paths on every call. `harvest_git_path_keeps_inherited_entries_after_standard_dirs` tests `git_path()` directly.

`git_deadline(repo)` returns the constant `GIT_COMMAND_TIMEOUT` of 30 seconds in production, and a test may register a shorter deadline per fixture repository through the `git_override` registry. A timeout fails the repository's scan, the repository keeps its earlier rows, and rows are stored only when the whole repository harvests. A repository that has never completed a scan therefore has no stored leave days to reuse, walks every archived task again on the next Update, and times out the same way each time. `ARCHITECTURE.md` names the constant under **Load-Bearing Decisions** ("kills it once `GIT_COMMAND_TIMEOUT` (30 seconds) passes") and lists `TASK_VIEWER_DB` and `TASK_VIEWER_SCAN_WORKERS` under **System Overview**. `scan_worker_cap` in `task-viewer/src/web.rs` is the precedent for parsing such a variable.

## Approach

1. In the buffered runner, have the stdout reader thread signal a channel when it reaches end of file. The caller waits on that channel with `recv_timeout` for the time left before the deadline, then polls `try_wait` with a short backoff for the rest of the deadline, because a child can close stdout and keep running. A deadline that passes in either phase kills and reaps the child and returns `GitFailure::Timeout` as today.
2. In `run_git_streaming`, replace the watcher's sleep loop with the same two phases: the watcher waits on a channel that the calling thread signals once the parser returns or aborts, killing the child when the deadline passes first, then polls `try_wait` for the rest of the deadline. The `abort` path keeps killing the child and returning `GitFailure::Other`.
3. Keep `git_path()` as the builder and have `scrub_git_env` read a `std::sync::LazyLock<OsString>` built from it. Resolve `git_bin()` once into a `LazyLock<&'static str>`. The test override in `git_program` is still consulted before the cached value.
4. Add a pure `git_timeout(raw: Option<&str>) -> Duration` beside `GIT_COMMAND_TIMEOUT`, modelled on `scan_worker_cap`: unset returns 30 seconds; a whole number of seconds from 1 to 600 is used as given; a larger value or an overflow clamps to 600; zero, negative, and non-numeric values log a warning naming `TASK_VIEWER_GIT_TIMEOUT_SECS` and return 30 seconds. Production `git_deadline` reads the variable once through a `LazyLock` in its `#[cfg(not(test))]` branch, so the test build keeps its current shape (the registered `git_override` deadline first, `GIT_COMMAND_TIMEOUT` otherwise) and `git_deadline_defaults_to_the_production_timeout` stays independent of the ambient environment. Rewrite the doc comment above `git_deadline` and the `GIT_COMMAND_TIMEOUT` comment so they name the constant as the default and the variable as the raise.
5. Rewrite the `GIT_COMMAND_TIMEOUT` sentence under **Load-Bearing Decisions** in `ARCHITECTURE.md` so it names the 30-second default and the variable. Rewrite the **System Overview** sentence that lists `TASK_VIEWER_DB` and `TASK_VIEWER_SCAN_WORKERS` so it also names the new variable and `RUST_LOG`, which `main.rs` already honours with a default filter of `task_viewer=info` and which no document mentions yet. Add the new variable to the **Update** bullet of `FEATURES.md` that names the worker cap, and to the `README.md` sentence that documents `TASK_VIEWER_SCAN_WORKERS`.

**Out of scope:** Storing a partial harvest when one git command times out, since `ARCHITECTURE.md` makes a timeout fail the whole repository's scan.

## Acceptance

- Neither runner in `task-viewer/src/harvest.rs` contains a `sleep(Duration::from_millis(5))` poll loop, checked with `rg -n 'from_millis\(5\)' task-viewer/src/harvest.rs` outside test code.
- A source-shape test in `task-viewer/src/harvest.rs`, in the shape of `every_log_and_cat_file_command_streams_child_stdout` (`include_str!("harvest.rs")` with `function_body`, called as `function_body(production, "run_git_command")` and `function_body(production, "run_git_streaming<T, F>")`, because the helper builds its needle as `format!("\nfn {name}(")` and `run_git_streaming`'s generic parameter list sits between its name and its opening parenthesis), asserts that the `run_git_command` and the `run_git_streaming` body each contain a `send(` on a channel and a `recv_timeout` on it, and that no `std::thread::sleep` call appears earlier in either function body than its first `recv_timeout`, so a runner that swaps one poll interval for another fails the test.
- A harvest test registers a stand-in git that closes its stdout and then sleeps for 10 seconds, with a 100 ms deadline, and gets `GitFailure::Timeout` for both the buffered and the streaming runner within a bound well under the sleep.
- The existing tests that hang a stand-in git with `exec sleep 10` under a short registered deadline still return `GitFailure::Timeout`, and the existing tests that read real fixture repositories still pass.
- A unit test of `git_timeout` covers unset, `45`, `600`, `601`, an overflowing value, `0`, `-3`, and `abc`, and asserts 30 s, 45 s, 600 s, 600 s, 600 s, 30 s, 30 s, and 30 s.
- A source-shape test in the house style of `every_log_and_cat_file_command_streams_child_stdout` reads `git_deadline`'s body from `include_str!("harvest.rs")` with `function_body`, and asserts that the text after its `#[cfg(not(test))]` marker reads the process-wide `LazyLock` value built from `git_timeout` and names no `GIT_COMMAND_TIMEOUT`, so the parsed `TASK_VIEWER_GIT_TIMEOUT_SECS` value reaches the deadline both runners use.
- `scrub_git_env` and `git_program` call no function that stats a path or rebuilds `PATH` per command, checked by reading them, and `harvest_git_path_keeps_inherited_entries_after_standard_dirs` still passes.
- `ARCHITECTURE.md`, `FEATURES.md`, and `README.md` each name `TASK_VIEWER_GIT_TIMEOUT_SECS` with its 30-second default, the **System Overview** sentence in `ARCHITECTURE.md` also names `RUST_LOG` and its default filter, and the old sentence that names only the fixed constant is gone.
