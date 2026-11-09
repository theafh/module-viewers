---
description: "Run Update's per-repository harvests in parallel, capped by TASK_VIEWER_SCAN_WORKERS (default 4), while database writes stay one repository at a time."
scope: task-viewer
created: 2026-10-24T14:47:55Z
updated: 2026-10-31T14:41:09Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: true
---

# Parallel Update with a configurable worker cap

## Goal

Make Update scan several configured repositories at once, with no more than a configured number of harvests running at the same time, so the whole Update takes about as long as its slowest repository rather than the sum of all of them. The cap comes from the environment variable `TASK_VIEWER_SCAN_WORKERS`, defaults to 4, and is at least 1. Every repository still ends Update with the same stored rows and scan errors a one-at-a-time scan would produce.

## Context

- `async fn scan_all` in `task-viewer/src/web.rs` handles `POST /scan`. It holds the scan lock, lists the repositories, and awaits `async fn refresh_one` for each repository in turn. `refresh_one` runs `harvest::harvest_repository` inside `tokio::task::spawn_blocking`, then writes the result through `db::replace_tasks` or records a message through `db::set_scan_error`.
- `pub async fn replace_tasks` in `task-viewer/src/db.rs` replaces one repository's rows inside a transaction. The pool is built with `SqlitePoolOptions` (`.max_connections(4)`), WAL journal mode and `.busy_timeout(Duration::from_secs(5))`. Writes that overlap would queue on SQLite's write lock, and one that waits past the busy timeout fails with `SQLITE_BUSY`. That would leave its repository with old rows and a scan error. This is why the writes stay sequential.
- Harvest is read-only. It runs `git log` lookups that take no repository lock, and its lookups inside one repository run one after another (`fn leave_dates`). Harvests of different repositories share no state, so running them together changes no result.
- The comment on `fn leave_dates` records that parallel lookups once exhausted process slots. That cause is unverified. The per-user process limit (`sysctl kern.maxprocperuid`) is far above the handful of `git` processes a small cap allows, and the cap is what keeps the process count bounded.
- A recent Update over the configured repositories took about 19 s. The slowest single repository took about 6 s, so a cap of 4 should bring Update close to that figure.
- `TASK_VIEWER_DB` (read in `db.rs`, described in `ARCHITECTURE.md`) is the existing precedent for configuring the binary through an environment variable. The launcher `scripts/task-viewer.sh` passes its environment through to the server.

## Approach

1. Add a pure function that turns the optional raw value of `TASK_VIEWER_SCAN_WORKERS` into the worker cap. An unset variable gives 4. A whole number from 1 to `MAX_SCAN_WORKERS` gives that number. A whole number above `MAX_SCAN_WORKERS` gives `MAX_SCAN_WORKERS`. `MAX_SCAN_WORKERS` is 32, which keeps the concurrent `git` subprocess count far below the hundreds that once exhausted process slots while leaving headroom over the default of 4. `0`, a negative value, or text that is not a number gives 4 and logs a `tracing::warn!` that names the rejected value. Read the variable once at startup and keep the cap in `AppState`.
2. Split the scan into two phases in `web.rs`. The harvest phase starts one `spawn_blocking` harvest per repository, gated by a `tokio::sync::Semaphore` with as many permits as the cap and collected in a `tokio::task::JoinSet`. The permit is acquired before `spawn_blocking` is called, so the blocking threads and the `git` subprocesses in flight are bounded by the cap. The store phase is the single loop that takes each finished result from the `JoinSet` and writes it with the same `replace_tasks` / `set_scan_error` outcomes and messages `refresh_one` uses today. Because only that loop writes, at most one write is in flight at any time.
3. Put the scan core in a function that takes the pool, the repository list, the cap and the harvest function as parameters. `scan_all` calls the scan core once with the whole repository list from `db::list_repositories`, `harvest::harvest_repository` and the cap, and the tests pass an instrumented stand-in. Keep the scan lock in `scan_all` as it is.
4. Rewrite the passages that describe Update in place:
   - In `ARCHITECTURE.md`, the `web.rs` paragraph states that Update harvests up to the cap at once and stores results one repository at a time. The sentence introducing `TASK_VIEWER_DB` gains `TASK_VIEWER_SCAN_WORKERS` with its default.
   - In `FEATURES.md`, the first sentence of the Repositories bullet, "Update on Dashboard scans every configured repository and replaces the parsed rows that still read cleanly.", is rewritten in place as "Update scans every configured repository, up to `TASK_VIEWER_SCAN_WORKERS` at once (default 4), and replaces the parsed rows that still read cleanly."
   - `README.md` gains one line naming the variable and its default next to the run instructions.
   - The `What Must Be Covered` list in `TESTING.md` gains the scan behaviours that Acceptance proves.

**Out of scope:**

- Parallel `git` lookups inside one repository, since `fn leave_dates` keeps those sequential and the cap here applies to whole repositories.
- The add-repository path, which harvests a single repository and keeps calling `refresh_one`.

## Acceptance

- [ ] A unit test of the cap function covers five inputs: unset gives 4, `"3"` gives 3, `"0"` gives 4, `"many"` gives 4, and `"300"` gives 32.
- [ ] A test runs the scan core with a cap of 2 over four repositories and a stand-in harvest that blocks for a fixed interval and records how many harvests are running at once. The recorded maximum never exceeds 2 and reaches 2.
- [ ] A test runs the scan core with a cap of 2 over at least three temporary repositories built with `unique_temp_dir`. Afterwards every repository's rows are stored and none has a scan error.
- [ ] A test runs the scan core over three repositories with a cap of 2, where one repository's folder has no `tasks/` directory and already holds rows from an earlier store. Afterwards that repository still holds those earlier rows and carries exactly "That folder has no tasks directory." as its scan error, while the other two store their rows with no scan error.
- [ ] `rg -n 'replace_tasks|set_scan_error' task-viewer/src/web.rs` shows every hit inside the scan core's store loop or in `refresh_one`, and none of them inside a spawned task.
- [ ] `scan_all` calls the scan core once with the repository list from `db::list_repositories`, `harvest::harvest_repository`, and the cap held in `AppState`, and contains no per-repository loop.
- [ ] `FEATURES.md` no longer carries the old Update bullet. Its replacement and the `ARCHITECTURE.md` paragraph each name `TASK_VIEWER_SCAN_WORKERS` and the default of 4 once, and the `FEATURES.md` replacement still states that Update covers every configured repository and replaces the parsed rows that still read cleanly.
- [ ] `README.md` names the variable and its default, and the `TESTING.md` `What Must Be Covered` list includes the cap parsing and the parallel scan.
