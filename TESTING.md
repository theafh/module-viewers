# Testing

How module-viewers is verified. `task-viewer` is a Rust binary on axum and sqlx. Design is `ARCHITECTURE.md`, behaviour is `FEATURES.md`, and constraints are `SECURITY.md`.

## Test Design Principles

- Test observable behaviour: parsed rows, chart series, HTTP host checks, and database contents. Where the behaviour depends on process-wide state that a parallel-safe test must not change, such as the inherited git environment or the server's working directory, assert how harvest configures its `git` command instead.
- Keep tests deterministic, isolated, offline, and parallel-safe. Every test owns its state through a per-test temp dir from `unique_temp_dir` (`task-viewer_<name>_<pid>_<nanos>_<n>`). Tests share no paths, no env overrides, and no execution-order assumptions. `cargo test` runs them in parallel.
- Cover the success path and one failure path for a behaviour that can fail.
- Match a status, a variant, or a field. Avoid asserting a whole error string when a variant is enough, and assert the stored message when that message is the contract.

## Test Organization

- Tests live in `#[cfg(test)]` modules in the source file they exercise.
- Shared helpers live in `#[cfg(test)]` `src/test_support.rs` (`unique_temp_dir`). There is no `[dev-dependencies]` block and no external fixture crate.
- Database tests set a thread-local database path under a unique temp dir and use the current-thread runtime so that path stays on the test thread.
- Git tests build a unique temporary repository with `user.email` and `user.name` passed on the command, and with author and committer dates in the environment.

## What Must Be Covered

- Date parsing for frontmatter timestamps and commit timestamps.
- Harvest: leave day is the first commit that archives the task with `status: finished`, `status: implemented` as the archived spelling a later commit may rename to `finished`, or `status: deferred`. For a finished task that means the earliest commit whose path lies under `archive` and whose content carries either status, including across renames after that commit: a task archived as `implemented` and later rewritten to `finished` dates from the earlier commit, as does a task written `status:finished` without a space; a task set to `implemented` while live and archived as `finished` dates from the archive commit; a task committed under `tasks/` as `finished` and later moved into `archive/` changing only its path dates from the move, and a later `updated` bump does not become the ship day; a merge commit that archives the task dates from the merge; an archived task deleted and restored, including a deletion on a branch reached through `--all`, keeps the first archived day and returns no `HarvestError`; a nested folder without its own `.git` dates from the enclosing repository's archived commit, and an `archive` directory above that folder does not count as archiving its live tasks; a SHA-256 repository dates its archived tasks the same way. The walk reads raw paths: a task file name holding a non-ASCII character or a double quote dates from its first archived commit, and so does a task in a nested folder whose name holds a non-ASCII character or a double quote, with no `HarvestError`; a walk record whose path holds a newline is skipped, so a task renamed to such a name keeps its first archived day and returns no `HarvestError`; and the walk parser keeps the next record after a path that is not valid UTF-8. Finished ship day equals that leave day; an inherited `GIT_DIR` must not poison the lookup; a date line in the patch context of the status change must not become the ship day; `--git-dir` lookups run from the repository folder, whatever the server's working directory; a `.git` that git cannot read fails the scan instead of falling back to `updated`; a folder outside git falls back to `updated`; deferred tasks have no ship day; live tasks stay live; a folder without `tasks/` is rejected.
- Harvest ref: a task archived on a branch that is not checked out still counts and keeps that commit's ship day; a task archived on one branch while the checked-out branch holds it live counts once, as the checked-out version; an uncommitted archive counts and falls back to the frontmatter `updated` day. The ref listing reads raw paths: a branch-only task whose file name holds a non-ASCII character or a double quote is charted with that branch's commit day as its ship day, with no `HarvestError`.
- Series: same-week create and ship both count, deferred tasks age until leave and never ship, all-projects radars count by repository, and a single project counts by scope.
- Database: insert, replace rows, keep rows when a scan error is recorded, and delete a repository with its rows.
- Scan: the worker cap parses unset, in-range, zero, non-numeric, and above-maximum values; Update harvests up to the cap at once, stores every repository's rows, and keeps an earlier store with the missing-tasks error when a folder has no `tasks/` directory.
- Host check: loopback names are accepted with or without port 7880.

## Running the Suite

- `make format` runs rustfmt.
- `make lint` runs clippy with warnings denied.
- `make test` runs `cargo test` with warnings denied.
- `make audit` runs `cargo audit` when dependencies change.
