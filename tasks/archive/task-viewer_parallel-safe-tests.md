---
description: "Make the inline suite parallel-safe with Aurora-style unique_temp_dir scratch paths and hermetic git fixtures."
scope: task-viewer
created: 2026-10-23T16:23:16
updated: 2026-10-23T16:23:16
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: false
---

# Parallel-safe test isolation

## Goal

Stop flaky `make test` failures under `cargo test` parallelism by giving every test its own scratch path, matching Aurora's isolation pattern, and keep harvest git fixtures hermetic so init does not depend on writing shared template hooks under temp.

The suite from [task-viewer_initial-dashboard.md](task-viewer_initial-dashboard.md) already covered behaviour; this task hardens how those fixtures own disk state.

## Context

- Failure mode: harvest tests used a nanos-only temp path (`task-viewer-repo-{nanos}`). Parallel cases that shared a stamp deleted each other's repo mid-run (`git -C …: No such file or directory`).
- Shared helper: `task-viewer/src/test_support.rs` (`unique_temp_dir` / `unique_temp_dir_in`).
- Call sites: harvest scratch dirs and the DB test database path under a unique temp dir.
- Methodology: root `TESTING.md` (deterministic, isolated, offline, parallel-safe).

## Approach

1. Add `#[cfg(test)]` `test_support` with `unique_temp_dir` paths shaped `task-viewer_<name>_<pid>_<nanos>_<n>` (name plus process id, clock, and atomic counter).
2. Point harvest fixtures at named unique dirs (`harvest_ship`, `harvest_missing`) and the DB test at a unique dir holding `test.db`.
3. Init fixture repos with `git init --template=` so empty templates skip host hook installs under temp.
4. Rewrite `TESTING.md` so parallel-safe ownership via `unique_temp_dir` is the recorded rule.

**Out of scope:**

- Changing harvest, series, or database production behaviour.
- Adding a `tests/` integration tree or `[dev-dependencies]` fixture crates.

## Acceptance

- [x] `task-viewer/src/test_support.rs` provides `unique_temp_dir` / `unique_temp_dir_in` and is wired from `main.rs` under `cfg(test)`.
- [x] Harvest and DB tests create scratch state only under those unique paths; no nanos-only shared temp name remains.
- [x] Harvest ship fixture uses `git init … --template=`.
- [x] `TESTING.md` states parallel-safe isolation and points at `unique_temp_dir`.
- [x] `make test` passes under default parallel `cargo test` (including repeated stress runs).
