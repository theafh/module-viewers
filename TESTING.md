# Testing

How module-viewers is verified. `task-viewer` is a Rust binary on axum and sqlx. Design is `ARCHITECTURE.md`, behaviour is `FEATURES.md`, and constraints are `SECURITY.md`.

## Test Design Principles

- Test observable behaviour: parsed rows, chart series, HTTP host checks, and database contents.
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
- Harvest: leave day is the first commit that introduces the archived status (`finished` or `deferred`), including across renames after that commit and when that commit is a merge; finished ship day equals that leave day; a later `updated` bump must not become the ship day; an inherited `GIT_DIR` must not poison pickaxe; a date line in the patch context of the status change must not become the ship day; `--git-dir` lookups run from the repository folder, whatever the server's working directory; deferred tasks have no ship day; live tasks stay live; a folder without `tasks/` is rejected.
- Series: same-week create and ship both count, deferred tasks age until leave and never ship, all-projects radars count by repository, and a single project counts by scope.
- Database: insert, replace rows, keep rows when a scan error is recorded, and delete a repository with its rows.
- Host check: loopback names are accepted with or without port 7880.

## Running the Suite

- `make format` runs rustfmt.
- `make lint` runs clippy with warnings denied.
- `make test` runs `cargo test` with warnings denied.
- `make audit` runs `cargo audit` when dependencies change.
